#![cfg_attr(
    not(feature = "agave-unstable-api"),
    deprecated(
        since = "3.1.0",
        note = "This crate has been marked for formal inclusion in the Agave Unstable API. From \
                v4.0.0 onward, the `agave-unstable-api` crate feature must be specified to \
                acknowledge use of an interface that may break without warning."
    )
)]
use {
    solana_bincode::limited_deserialize,
    solana_bpf_loader_program::{deploy_program, execute},
    solana_instruction::error::InstructionError,
    solana_loader_v3_interface::state::UpgradeableLoaderState,
    solana_loader_v4_interface::{
        instruction::LoaderV4Instruction,
        state::{LoaderV4State, LoaderV4Status},
        DEPLOYMENT_COOLDOWN_IN_SLOTS,
    },
    solana_program_runtime::{
        invoke_context::InvokeContext,
        loaded_programs::{ProgramCacheEntry, ProgramCacheEntryOwner, ProgramCacheEntryType},
    },
    solana_pubkey::Pubkey,
    solana_sbpf::{declare_builtin_function, memory_region::MemoryMapping},
    solana_sdk_ids::{bpf_loader, bpf_loader_deprecated, bpf_loader_upgradeable, loader_v4},
    solana_svm_log_collector::{ic_logger_msg, LogCollector},
    solana_svm_measure::measure::Measure,
    solana_svm_type_overrides::sync::Arc,
    solana_transaction_context::{
        instruction::InstructionContext, instruction_accounts::BorrowedInstructionAccount,
    },
    std::{cell::RefCell, rc::Rc},
};

pub const DEFAULT_COMPUTE_UNITS: u64 = 2_000;

pub fn get_state(data: &[u8]) -> Result<&LoaderV4State, InstructionError> {
    unsafe {
        let data = data
            .get(0..LoaderV4State::program_data_offset())
            .ok_or(InstructionError::AccountDataTooSmall)?
            .try_into()
            .unwrap();
        Ok(std::mem::transmute::<
            &[u8; LoaderV4State::program_data_offset()],
            &LoaderV4State,
        >(data))
    }
}

fn get_state_mut(data: &mut [u8]) -> Result<&mut LoaderV4State, InstructionError> {
    unsafe {
        let data = data
            .get_mut(0..LoaderV4State::program_data_offset())
            .ok_or(InstructionError::AccountDataTooSmall)?
            .try_into()
            .unwrap();
        Ok(std::mem::transmute::<
            &mut [u8; LoaderV4State::program_data_offset()],
            &mut LoaderV4State,
        >(data))
    }
}

fn check_program_account(
    log_collector: &Option<Rc<RefCell<LogCollector>>>,
    instruction_context: &InstructionContext,
    program: &BorrowedInstructionAccount,
    authority_address: &Pubkey,
) -> Result<LoaderV4State, InstructionError> {
    if !loader_v4::check_id(program.get_owner()) {
        ic_logger_msg!(log_collector, "Program not owned by loader");
        return Err(InstructionError::InvalidAccountOwner);
    }
    let state = get_state(program.get_data())?;
    if !program.is_writable() {
        ic_logger_msg!(log_collector, "Program is not writeable");
        return Err(InstructionError::InvalidArgument);
    }
    if !instruction_context.is_instruction_account_signer(1)? {
        ic_logger_msg!(log_collector, "Authority did not sign");
        return Err(InstructionError::MissingRequiredSignature);
    }
    if state.authority_address_or_next_version != *authority_address {
        ic_logger_msg!(log_collector, "Incorrect authority provided");
        return Err(InstructionError::IncorrectAuthority);
    }
    if matches!(state.status, LoaderV4Status::Finalized) {
        ic_logger_msg!(log_collector, "Program is finalized");
        return Err(InstructionError::Immutable);
    }
    Ok(*state)
}

fn process_instruction_write(
    invoke_context: &mut InvokeContext,
    offset: u32,
    bytes: Vec<u8>,
) -> Result<(), InstructionError> {
    let log_collector = invoke_context.get_log_collector();
    let transaction_context = &invoke_context.transaction_context;
    let instruction_context = transaction_context.get_current_instruction_context()?;
    let mut program = instruction_context.try_borrow_instruction_account(0)?;
    let authority_address = instruction_context.get_key_of_instruction_account(1)?;
    let state = check_program_account(
        &log_collector,
        &instruction_context,
        &program,
        authority_address,
    )?;
    if !matches!(state.status, LoaderV4Status::Retracted) {
        ic_logger_msg!(log_collector, "Program is not retracted");
        return Err(InstructionError::InvalidArgument);
    }
    let destination_offset = (offset as usize).saturating_add(LoaderV4State::program_data_offset());
    program
        .get_data_mut()?
        .get_mut(destination_offset..destination_offset.saturating_add(bytes.len()))
        .ok_or_else(|| {
            ic_logger_msg!(log_collector, "Write out of bounds");
            InstructionError::AccountDataTooSmall
        })?
        .copy_from_slice(&bytes);
    Ok(())
}

fn process_instruction_copy(
    invoke_context: &mut InvokeContext,
    destination_offset: u32,
    source_offset: u32,
    length: u32,
) -> Result<(), InstructionError> {
    let log_collector = invoke_context.get_log_collector();
    let transaction_context = &invoke_context.transaction_context;
    let instruction_context = transaction_context.get_current_instruction_context()?;
    let mut program = instruction_context.try_borrow_instruction_account(0)?;
    let authority_address = instruction_context.get_key_of_instruction_account(1)?;
    let source_program = instruction_context.try_borrow_instruction_account(2)?;
    let state = check_program_account(
        &log_collector,
        &instruction_context,
        &program,
        authority_address,
    )?;
    if !matches!(state.status, LoaderV4Status::Retracted) {
        ic_logger_msg!(log_collector, "Program is not retracted");
        return Err(InstructionError::InvalidArgument);
    }
    let source_owner = &source_program.get_owner();
    let source_offset =
        (source_offset as usize).saturating_add(if loader_v4::check_id(source_owner) {
            LoaderV4State::program_data_offset()
        } else if bpf_loader_upgradeable::check_id(source_owner) {
            UpgradeableLoaderState::size_of_programdata_metadata()
        } else if bpf_loader_deprecated::check_id(source_owner)
            || bpf_loader::check_id(source_owner)
        {
            0
        } else {
            ic_logger_msg!(log_collector, "Source is not a program");
            return Err(InstructionError::InvalidArgument);
        });
    let data = source_program
        .get_data()
        .get(source_offset..source_offset.saturating_add(length as usize))
        .ok_or_else(|| {
            ic_logger_msg!(log_collector, "Read out of bounds");
            InstructionError::AccountDataTooSmall
        })?;
    let destination_offset =
        (destination_offset as usize).saturating_add(LoaderV4State::program_data_offset());
    program
        .get_data_mut()?
        .get_mut(destination_offset..destination_offset.saturating_add(length as usize))
        .ok_or_else(|| {
            ic_logger_msg!(log_collector, "Write out of bounds");
            InstructionError::AccountDataTooSmall
        })?
        .copy_from_slice(data);
    Ok(())
}

fn process_instruction_set_program_length(
    invoke_context: &mut InvokeContext,
    new_size: u32,
) -> Result<(), InstructionError> {
    let log_collector = invoke_context.get_log_collector();
    let transaction_context = &invoke_context.transaction_context;
    let instruction_context = transaction_context.get_current_instruction_context()?;
    let mut program = instruction_context.try_borrow_instruction_account(0)?;
    let authority_address = instruction_context.get_key_of_instruction_account(1)?;
    let is_initialization = program.get_data().len() < LoaderV4State::program_data_offset();
    if is_initialization {
        if !loader_v4::check_id(program.get_owner()) {
            ic_logger_msg!(log_collector, "Program not owned by loader");
            return Err(InstructionError::InvalidAccountOwner);
        }
        if !program.is_writable() {
            ic_logger_msg!(log_collector, "Program is not writeable");
            return Err(InstructionError::InvalidArgument);
        }
        if !instruction_context.is_instruction_account_signer(1)? {
            ic_logger_msg!(log_collector, "Authority did not sign");
            return Err(InstructionError::MissingRequiredSignature);
        }
    } else {
        let state = check_program_account(
            &log_collector,
            &instruction_context,
            &program,
            authority_address,
        )?;
        if !matches!(state.status, LoaderV4Status::Retracted) {
            ic_logger_msg!(log_collector, "Program is not retracted");
            return Err(InstructionError::InvalidArgument);
        }
    }
    let required_lamports = if new_size == 0 {
        0
    } else {
        let rent = invoke_context.get_sysvar_cache().get_rent()?;
        rent.minimum_balance(LoaderV4State::program_data_offset().saturating_add(new_size as usize))
            .max(1)
    };
    match program.get_lamports().cmp(&required_lamports) {
        std::cmp::Ordering::Less => {
            ic_logger_msg!(
                log_collector,
                "Insufficient lamports, {} are required",
                required_lamports
            );
            return Err(InstructionError::InsufficientFunds);
        }
        std::cmp::Ordering::Greater => {
            let recipient = instruction_context.try_borrow_instruction_account(2).ok();
            if let Some(mut recipient) = recipient {
                if !instruction_context.is_instruction_account_writable(2)? {
                    ic_logger_msg!(log_collector, "Recipient is not writeable");
                    return Err(InstructionError::InvalidArgument);
                }
                let lamports_to_receive = program.get_lamports().saturating_sub(required_lamports);
                program.checked_sub_lamports(lamports_to_receive)?;
                recipient.checked_add_lamports(lamports_to_receive)?;
            } else if new_size == 0 {
                ic_logger_msg!(
                    log_collector,
                    "Closing a program requires a recipient account"
                );
                return Err(InstructionError::InvalidArgument);
            }
        }
        std::cmp::Ordering::Equal => {}
    }
    if new_size == 0 {
        program.set_data_length(0)?;
    } else {
        program.set_data_length(
            LoaderV4State::program_data_offset().saturating_add(new_size as usize),
        )?;
        if is_initialization {
            program.set_executable(true)?;
            let state = get_state_mut(program.get_data_mut()?)?;
            state.slot = 0;
            state.status = LoaderV4Status::Retracted;
            state.authority_address_or_next_version = *authority_address;
        }
    }
    Ok(())
}

fn process_instruction_deploy(invoke_context: &mut InvokeContext) -> Result<(), InstructionError> {
    let log_collector = invoke_context.get_log_collector();
    let transaction_context = &invoke_context.transaction_context;
    let instruction_context = transaction_context.get_current_instruction_context()?;
    let mut program = instruction_context.try_borrow_instruction_account(0)?;
    let authority_address = instruction_context.get_key_of_instruction_account(1)?;
    let state = check_program_account(
        &log_collector,
        &instruction_context,
        &program,
        authority_address,
    )?;
    let current_slot = invoke_context.get_sysvar_cache().get_clock()?.slot;

    // Slot = 0 indicates that the program hasn't been deployed yet. So no need to check for the cooldown slots.
    // (Without this check, the program deployment is failing in freshly started test validators. That's
    //  because at startup current_slot is 0, which is < DEPLOYMENT_COOLDOWN_IN_SLOTS).
    if state.slot != 0 && state.slot.saturating_add(DEPLOYMENT_COOLDOWN_IN_SLOTS) > current_slot {
        ic_logger_msg!(
            log_collector,
            "Program was deployed recently, cooldown still in effect"
        );
        return Err(InstructionError::InvalidArgument);
    }
    if !matches!(state.status, LoaderV4Status::Retracted) {
        ic_logger_msg!(log_collector, "Destination program is not retracted");
        return Err(InstructionError::InvalidArgument);
    }

    let programdata = program
        .get_data()
        .get(LoaderV4State::program_data_offset()..)
        .ok_or(InstructionError::AccountDataTooSmall)?;
    deploy_program!(
        invoke_context,
        program.get_key(),
        &loader_v4::id(),
        program.get_data().len(),
        programdata,
        current_slot,
    );

    let state = get_state_mut(program.get_data_mut()?)?;
    state.slot = current_slot;
    state.status = LoaderV4Status::Deployed;
    Ok(())
}

fn process_instruction_retract(invoke_context: &mut InvokeContext) -> Result<(), InstructionError> {
    let log_collector = invoke_context.get_log_collector();
    let transaction_context = &invoke_context.transaction_context;
    let instruction_context = transaction_context.get_current_instruction_context()?;
    let mut program = instruction_context.try_borrow_instruction_account(0)?;

    let authority_address = instruction_context.get_key_of_instruction_account(1)?;
    let state = check_program_account(
        &log_collector,
        &instruction_context,
        &program,
        authority_address,
    )?;
    let current_slot = invoke_context.get_sysvar_cache().get_clock()?.slot;
    if state.slot.saturating_add(DEPLOYMENT_COOLDOWN_IN_SLOTS) > current_slot {
        ic_logger_msg!(
            log_collector,
            "Program was deployed recently, cooldown still in effect"
        );
        return Err(InstructionError::InvalidArgument);
    }
    if !matches!(state.status, LoaderV4Status::Deployed) {
        ic_logger_msg!(log_collector, "Program is not deployed");
        return Err(InstructionError::InvalidArgument);
    }
    let state = get_state_mut(program.get_data_mut()?)?;
    state.status = LoaderV4Status::Retracted;
    invoke_context
        .program_cache_for_tx_batch
        .store_modified_entry(
            *program.get_key(),
            Arc::new(ProgramCacheEntry::new_tombstone(
                current_slot,
                ProgramCacheEntryOwner::LoaderV4,
                ProgramCacheEntryType::Closed,
            )),
        );
    Ok(())
}

fn process_instruction_transfer_authority(
    invoke_context: &mut InvokeContext,
) -> Result<(), InstructionError> {
    let log_collector = invoke_context.get_log_collector();
    let transaction_context = &invoke_context.transaction_context;
    let instruction_context = transaction_context.get_current_instruction_context()?;
    let mut program = instruction_context.try_borrow_instruction_account(0)?;
    let authority_address = instruction_context.get_key_of_instruction_account(1)?;
    let new_authority_address = instruction_context.get_key_of_instruction_account(2)?;
    let state = check_program_account(
        &log_collector,
        &instruction_context,
        &program,
        authority_address,
    )?;
    if !instruction_context.is_instruction_account_signer(2)? {
        ic_logger_msg!(log_collector, "New authority did not sign");
        return Err(InstructionError::MissingRequiredSignature);
    }
    if state.authority_address_or_next_version == *new_authority_address {
        ic_logger_msg!(log_collector, "No change");
        return Err(InstructionError::InvalidArgument);
    }
    let state = get_state_mut(program.get_data_mut()?)?;
    state.authority_address_or_next_version = *new_authority_address;
    Ok(())
}

fn process_instruction_finalize(
    invoke_context: &mut InvokeContext,
) -> Result<(), InstructionError> {
    let log_collector = invoke_context.get_log_collector();
    let transaction_context = &invoke_context.transaction_context;
    let instruction_context = transaction_context.get_current_instruction_context()?;
    let program = instruction_context.try_borrow_instruction_account(0)?;
    let authority_address = instruction_context.get_key_of_instruction_account(1)?;
    let state = check_program_account(
        &log_collector,
        &instruction_context,
        &program,
        authority_address,
    )?;
    if !matches!(state.status, LoaderV4Status::Deployed) {
        ic_logger_msg!(log_collector, "Program must be deployed to be finalized");
        return Err(InstructionError::InvalidArgument);
    }
    drop(program);
    let next_version = instruction_context.try_borrow_instruction_account(2)?;
    if !loader_v4::check_id(next_version.get_owner()) {
        ic_logger_msg!(log_collector, "Next version is not owned by loader");
        return Err(InstructionError::InvalidAccountOwner);
    }
    let state_of_next_version = get_state(next_version.get_data())?;
    if state_of_next_version.authority_address_or_next_version != *authority_address {
        ic_logger_msg!(log_collector, "Next version has a different authority");
        return Err(InstructionError::IncorrectAuthority);
    }
    if matches!(state_of_next_version.status, LoaderV4Status::Finalized) {
        ic_logger_msg!(log_collector, "Next version is finalized");
        return Err(InstructionError::Immutable);
    }
    let address_of_next_version = *next_version.get_key();
    drop(next_version);
    let mut program = instruction_context.try_borrow_instruction_account(0)?;
    let state = get_state_mut(program.get_data_mut()?)?;
    state.authority_address_or_next_version = address_of_next_version;
    state.status = LoaderV4Status::Finalized;
    Ok(())
}

declare_builtin_function!(
    Entrypoint,
    fn rust(
        invoke_context: &mut InvokeContext<'static, 'static>,
        _arg0: u64,
        _arg1: u64,
        _arg2: u64,
        _arg3: u64,
        _arg4: u64,
        _memory_mapping: &mut MemoryMapping,
    ) -> Result<u64, Box<dyn std::error::Error>> {
        process_instruction_inner(invoke_context)
    }
);

fn process_instruction_inner<'a>(
    invoke_context: &mut InvokeContext<'a, 'a>,
) -> Result<u64, Box<dyn std::error::Error>> {
    let log_collector = invoke_context.get_log_collector();
    let transaction_context = &invoke_context.transaction_context;
    let instruction_context = transaction_context.get_current_instruction_context()?;
    let instruction_data = instruction_context.get_instruction_data();
    let program_id = instruction_context.get_program_key()?;
    if loader_v4::check_id(program_id) {
        invoke_context.consume_checked(DEFAULT_COMPUTE_UNITS)?;
        match limited_deserialize(instruction_data, solana_packet::PACKET_DATA_SIZE as u64)? {
            LoaderV4Instruction::Write { offset, bytes } => {
                process_instruction_write(invoke_context, offset, bytes)
            }
            LoaderV4Instruction::Copy {
                destination_offset,
                source_offset,
                length,
            } => {
                process_instruction_copy(invoke_context, destination_offset, source_offset, length)
            }
            LoaderV4Instruction::SetProgramLength { new_size } => {
                process_instruction_set_program_length(invoke_context, new_size)
            }
            LoaderV4Instruction::Deploy => process_instruction_deploy(invoke_context),
            LoaderV4Instruction::Retract => process_instruction_retract(invoke_context),
            LoaderV4Instruction::TransferAuthority => {
                process_instruction_transfer_authority(invoke_context)
            }
            LoaderV4Instruction::Finalize => process_instruction_finalize(invoke_context),
        }
        .map_err(|err| Box::new(err) as Box<dyn std::error::Error>)
    } else {
        let mut get_or_create_executor_time = Measure::start("get_or_create_executor_time");
        let loaded_program = invoke_context
            .program_cache_for_tx_batch
            .find(program_id)
            .ok_or_else(|| {
                ic_logger_msg!(log_collector, "Program is not cached");
                InstructionError::UnsupportedProgramId
            })?;
        get_or_create_executor_time.stop();
        invoke_context.timings.get_or_create_executor_us += get_or_create_executor_time.as_us();
        match &loaded_program.program {
            ProgramCacheEntryType::FailedVerification(_)
            | ProgramCacheEntryType::Closed
            | ProgramCacheEntryType::DelayVisibility => {
                ic_logger_msg!(log_collector, "Program is not deployed");
                Err(Box::new(InstructionError::UnsupportedProgramId) as Box<dyn std::error::Error>)
            }
            ProgramCacheEntryType::Loaded(executable) => execute(executable, invoke_context),
            _ => {
                Err(Box::new(InstructionError::UnsupportedProgramId) as Box<dyn std::error::Error>)
            }
        }
    }
    .map(|_| 0)
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_account::{
            create_account_shared_data_for_test, AccountSharedData, ReadableAccount,
            WritableAccount,
        },
        solana_bpf_loader_program::test_utils,
        solana_clock::Slot,
        solana_instruction::AccountMeta,
        solana_program_runtime::invoke_context::mock_process_instruction,
        solana_sysvar::{clock, rent},
        solana_transaction_context::IndexOfAccount,
        std::{fs::File, io::Read, path::Path},
    };

    fn process_instruction(
        program_index: Option<IndexOfAccount>,
        instruction_data: &[u8],
        transaction_accounts: Vec<(Pubkey, AccountSharedData)>,
        instruction_accounts: &[(IndexOfAccount, bool, bool)],
        expected_result: Result<(), InstructionError>,
    ) -> Vec<AccountSharedData> {
        let instruction_accounts = instruction_accounts
            .iter()
            .map(
                |(index_in_transaction, is_signer, is_writable)| AccountMeta {
                    pubkey: transaction_accounts[*index_in_transaction as usize].0,
                    is_signer: *is_signer,
                    is_writable: *is_writable,
                },
            )
            .collect::<Vec<_>>();

        mock_process_instruction(
            &loader_v4::id(),
            program_index,
            instruction_data,
            transaction_accounts,
            instruction_accounts,
            expected_result,
            Entrypoint::vm,
            |invoke_context| {
                test_utils::load_all_invoked_programs(invoke_context);
            },
            |_invoke_context| {},
        )
    }

    fn load_program_account_from_elf(
        authority_address: Pubkey,
        status: LoaderV4Status,
        path: &str,
    ) -> AccountSharedData {
        let path = Path::new("../bpf_loader/test_elfs/out/")
            .join(path)
            .with_extension("so");
        let mut file = File::open(path).expect("file open failed");
        let mut elf_bytes = Vec::new();
        file.read_to_end(&mut elf_bytes).unwrap();
        let rent = rent::Rent::default();
        let account_size = LoaderV4State::program_data_offset().saturating_add(elf_bytes.len());
        let mut program_account = AccountSharedData::new(
            rent.minimum_balance(account_size),
            account_size,
            &loader_v4::id(),
        );
        let state = get_state_mut(program_account.data_as_mut_slice()).unwrap();
        state.slot = 0;
        state.authority_address_or_next_version = authority_address;
        state.status = status;
        program_account.data_as_mut_slice()[LoaderV4State::program_data_offset()..]
            .copy_from_slice(&elf_bytes);
        program_account
    }

    fn clock(slot: Slot) -> AccountSharedData {
        let clock = clock::Clock {
            slot,
            ..clock::Clock::default()
        };
        create_account_shared_data_for_test(&clock)
    }

    fn test_loader_instruction_general_errors(instruction: LoaderV4Instruction) {
        let instruction = bincode::serialize(&instruction).unwrap();
        let authority_address = Pubkey::new_unique();
        let transaction_accounts = vec![
            (
                Pubkey::new_unique(),
                load_program_account_from_elf(
                    authority_address,
                    LoaderV4Status::Deployed,
                    "sbpfv3_return_err",
                ),
            ),
            (
                authority_address,
                AccountSharedData::new(0, 0, &Pubkey::new_unique()),
            ),
            (
                Pubkey::new_unique(),
                load_program_account_from_elf(
                    authority_address,
                    LoaderV4Status::Finalized,
                    "sbpfv3_return_err",
                ),
            ),
            (
                clock::id(),
                create_account_shared_data_for_test(&clock::Clock::default()),
            ),
            (
                rent::id(),
                create_account_shared_data_for_test(&rent::Rent::default()),
            ),
        ];

        // Error: Missing program account
        process_instruction(
            None,
            &instruction,
            transaction_accounts.clone(),
            &[],
            Err(InstructionError::MissingAccount),
        );

        // Error: Missing authority account
        process_instruction(
            None,
            &instruction,
            transaction_accounts.clone(),
            &[(0, false, true)],
            Err(InstructionError::MissingAccount),
        );

        // Error: Program not owned by loader
        process_instruction(
            None,
            &instruction,
            transaction_accounts.clone(),
            &[(1, false, true), (1, true, false), (2, true, true)],
            Err(InstructionError::InvalidAccountOwner),
        );

        // Error: Program is not writeable
        process_instruction(
            None,
            &instruction,
            transaction_accounts.clone(),
            &[(0, false, false), (1, true, false), (2, true, true)],
            Err(InstructionError::InvalidArgument),
        );

        // Error: Authority did not sign
        process_instruction(
            None,
            &instruction,
            transaction_accounts.clone(),
            &[(0, false, true), (1, false, false), (2, true, true)],
            Err(InstructionError::MissingRequiredSignature),
        );

        // Error: Program is finalized
        process_instruction(
            None,
            &instruction,
            transaction_accounts.clone(),
            &[(2, false, true), (1, true, false), (0, true, true)],
            Err(InstructionError::Immutable),
        );

        // Error: Incorrect authority provided
        process_instruction(
            None,
            &instruction,
            transaction_accounts,
            &[(0, false, true), (2, true, false), (2, true, true)],
            Err(InstructionError::IncorrectAuthority),
        );
    }

    #[test]
    fn test_loader_instruction_write() {
        let authority_address = Pubkey::new_unique();
        let transaction_accounts = vec![
            (
                Pubkey::new_unique(),
                load_program_account_from_elf(
                    authority_address,
                    LoaderV4Status::Retracted,
                    "sbpfv3_return_err",
                ),
            ),
            (
                authority_address,
                AccountSharedData::new(0, 0, &Pubkey::new_unique()),
            ),
            (
                Pubkey::new_unique(),
                load_program_account_from_elf(
                    authority_address,
                    LoaderV4Status::Deployed,
                    "sbpfv3_return_err",
                ),
            ),
            (
                clock::id(),
                create_account_shared_data_for_test(&clock::Clock::default()),
            ),
            (
                rent::id(),
                create_account_shared_data_for_test(&rent::Rent::default()),
            ),
        ];

        // Overwrite existing data
        process_instruction(
            None,
            &bincode::serialize(&LoaderV4Instruction::Write {
                offset: 2,
                bytes: vec![8, 8, 8, 8],
            })
            .unwrap(),
            transaction_accounts.clone(),
            &[(0, false, true), (1, true, false)],
            Ok(()),
        );

        // Empty write
        process_instruction(
            None,
            &bincode::serialize(&LoaderV4Instruction::Write {
                offset: 2,
                bytes: Vec::new(),
            })
            .unwrap(),
            transaction_accounts.clone(),
            &[(0, false, true), (1, true, false)],
            Ok(()),
        );

        // Error: Program is not retracted
        process_instruction(
            None,
            &bincode::serialize(&LoaderV4Instruction::Write {
                offset: 8,
                bytes: vec![8, 8, 8, 8],
            })
            .unwrap(),
            transaction_accounts.clone(),
            &[(2, false, true), (1, true, false)],
            Err(InstructionError::InvalidArgument),
        );

        // Error: Write out of bounds
        process_instruction(
            None,
            &bincode::serialize(&LoaderV4Instruction::Write {
                offset: transaction_accounts[0]
                    .1
                    .data()
                    .len()
                    .saturating_sub(LoaderV4State::program_data_offset())
                    .saturating_sub(3) as u32,
                bytes: vec![8, 8, 8, 8],
            })
            .unwrap(),
            transaction_accounts.clone(),
            &[(0, false, true), (1, true, false)],
            Err(InstructionError::AccountDataTooSmall),
        );

        test_loader_instruction_general_errors(LoaderV4Instruction::Write {
            offset: 0,
            bytes: Vec::new(),
        });
    }

    #[test]
    fn test_loader_instruction_copy() {
        let authority_address = Pubkey::new_unique();
        let transaction_accounts = vec![
            (
                Pubkey::new_unique(),
                load_program_account_from_elf(
                    authority_address,
                    LoaderV4Status::Retracted,
                    "sbpfv3_return_err",
                ),
            ),
            (
                authority_address,
                AccountSharedData::new(0, 0, &Pubkey::new_unique()),
            ),
            (
                Pubkey::new_unique(),
                load_program_account_from_elf(
                    authority_address,
                    LoaderV4Status::Deployed,
                    "sbpfv3_return_err",
                ),
            ),
            (
                clock::id(),
                create_account_shared_data_for_test(&clock::Clock::default()),
            ),
            (
                rent::id(),
                create_account_shared_data_for_test(&rent::Rent::default()),
            ),
        ];

        // Overwrite existing data
        process_instruction(
            None,
            &bincode::serialize(&LoaderV4Instruction::Copy {
                destination_offset: 1,
                source_offset: 2,
                length: 3,
            })
            .unwrap(),
            transaction_accounts.clone(),
            &[(0, false, true), (1, true, false), (2, false, false)],
            Ok(()),
        );

        // Empty copy
        process_instruction(
            None,
            &bincode::serialize(&LoaderV4Instruction::Copy {
                destination_offset: 1,
                source_offset: 2,
                length: 0,
            })
            .unwrap(),
            transaction_accounts.clone(),
            &[(0, false, true), (1, true, false), (2, false, false)],
            Ok(()),
        );

        // Error: Program is not retracted
        process_instruction(
            None,
            &bincode::serialize(&LoaderV4Instruction::Copy {
                destination_offset: 1,
                source_offset: 2,
                length: 3,
            })
            .unwrap(),
            transaction_accounts.clone(),
            &[(2, false, true), (1, true, false), (0, false, false)],
            Err(InstructionError::InvalidArgument),
        );

        // Error: Destination and source collide
        process_instruction(
            None,
            &bincode::serialize(&LoaderV4Instruction::Copy {
                destination_offset: 1,
                source_offset: 2,
                length: 3,
            })
            .unwrap(),
            transaction_accounts.clone(),
            &[(2, false, true), (1, true, false), (2, false, false)],
            Err(InstructionError::AccountBorrowFailed),
        );

        // Error: Read out of bounds
        process_instruction(
            None,
            &bincode::serialize(&LoaderV4Instruction::Copy {
                destination_offset: 1,
                source_offset: transaction_accounts[2]
                    .1
                    .data()
                    .len()
                    .saturating_sub(LoaderV4State::program_data_offset())
                    .saturating_sub(3) as u32,
                length: 4,
            })
            .unwrap(),
            transaction_accounts.clone(),
            &[(0, false, true), (1, true, false), (2, false, false)],
            Err(InstructionError::AccountDataTooSmall),
        );

        // Error: Write out of bounds
        process_instruction(
            None,
            &bincode::serialize(&LoaderV4Instruction::Copy {
                destination_offset: transaction_accounts[0]
                    .1
                    .data()
                    .len()
                    .saturating_sub(LoaderV4State::program_data_offset())
                    .saturating_sub(3) as u32,
                source_offset: 2,
                length: 4,
            })
            .unwrap(),
            transaction_accounts.clone(),
            &[(0, false, true), (1, true, false), (2, false, false)],
            Err(InstructionError::AccountDataTooSmall),
        );

        test_loader_instruction_general_errors(LoaderV4Instruction::Copy {
            destination_offset: 1,
            source_offset: 2,
            length: 3,
        });
    }

    #[test]
    fn test_loader_instruction_truncate() {
        let authority_address = Pubkey::new_unique();
        let mut transaction_accounts = vec![
            (
                Pubkey::new_unique(),
                load_program_account_from_elf(
                    authority_address,
                    LoaderV4Status::Retracted,
                    "sbpfv3_return_err",
                ),
            ),
            (
                authority_address,
                AccountSharedData::new(0, 0, &Pubkey::new_unique()),
            ),
            (
                Pubkey::new_unique(),
                AccountSharedData::new(0, 0, &loader_v4::id()),
            ),
            (
                Pubkey::new_unique(),
                AccountSharedData::new(0, 0, &loader_v4::id()),
            ),
            (
                Pubkey::new_unique(),
                load_program_account_from_elf(
                    authority_address,
                    LoaderV4Status::Retracted,
                    "sbpfv3_return_ok",
                ),
            ),
            (
                Pubkey::new_unique(),
                load_program_account_from_elf(
                    authority_address,
                    LoaderV4Status::Deployed,
                    "sbpfv3_return_err",
                ),
            ),
            (
                clock::id(),
                create_account_shared_data_for_test(&clock::Clock::default()),
            ),
            (
                rent::id(),
                create_account_shared_data_for_test(&rent::Rent::default()),
            ),
        ];
        let smaller_program_lamports = transaction_accounts[0].1.lamports();
        let larger_program_lamports = transaction_accounts[4].1.lamports();
        assert_ne!(smaller_program_lamports, larger_program_lamports);

        // No change
        let accounts = process_instruction(
            None,
            &bincode::serialize(&LoaderV4Instruction::SetProgramLength {
                new_size: transaction_accounts[0]
                    .1
                    .data()
                    .len()
                    .saturating_sub(LoaderV4State::program_data_offset())
                    as u32,
            })
            .unwrap(),
            transaction_accounts.clone(),
            &[(0, false, true), (1, true, false)],
            Ok(()),
        );
        assert_eq!(
            accounts[0].data().len(),
            transaction_accounts[0].1.data().len(),
        );
        assert_eq!(accounts[2].lamports(), transaction_accounts[2].1.lamports());

        // Initialize program account
        transaction_accounts[3]
            .1
            .set_lamports(smaller_program_lamports);
        let accounts = process_instruction(
            None,
            &bincode::serialize(&LoaderV4Instruction::SetProgramLength {
                new_size: transaction_accounts[0]
                    .1
                    .data()
                    .len()
                    .saturating_sub(LoaderV4State::program_data_offset())
                    as u32,
            })
            .unwrap(),
            transaction_accounts.clone(),
            &[(3, true, true), (1, true, false), (2, false, true)],
            Ok(()),
        );
        assert_eq!(
            accounts[3].data().len(),
            transaction_accounts[0].1.data().len(),
        );

        // Increase program account size
        transaction_accounts[0]
            .1
            .set_lamports(larger_program_lamports);
        let accounts = process_instruction(
            None,
            &bincode::serialize(&LoaderV4Instruction::SetProgramLength {
                new_size: transaction_accounts[4]
                    .1
                    .data()
                    .len()
                    .saturating_sub(LoaderV4State::program_data_offset())
                    as u32,
            })
            .unwrap(),
            transaction_accounts.clone(),
            &[(0, false, true), (1, true, false)],
            Ok(()),
        );
        assert_eq!(
            accounts[0].data().len(),
            transaction_accounts[4].1.data().len(),
        );

        // Decrease program account size, with a recipient
        let accounts = process_instruction(
            None,
            &bincode::serialize(&LoaderV4Instruction::SetProgramLength {
                new_size: transaction_accounts[0]
                    .1
                    .data()
                    .len()
                    .saturating_sub(LoaderV4State::program_data_offset())
                    as u32,
            })
            .unwrap(),
            transaction_accounts.clone(),
            &[(4, false, true), (1, true, false), (2, false, true)],
            Ok(()),
        );
        assert_eq!(
            accounts[4].data().len(),
            transaction_accounts[0].1.data().len(),
        );
        assert_eq!(
            accounts[2].lamports(),
            transaction_accounts[2].1.lamports().saturating_add(
                transaction_accounts[4]
                    .1
                    .lamports()
                    .saturating_sub(accounts[4].lamports())
            ),
        );

        // Decrease program account size, without a recipient
        let accounts = process_instruction(
            None,
            &bincode::serialize(&LoaderV4Instruction::SetProgramLength {
                new_size: transaction_accounts[0]
                    .1
                    .data()
                    .len()
                    .saturating_sub(LoaderV4State::program_data_offset())
                    as u32,
            })
            .unwrap(),
            transaction_accounts.clone(),
            &[(4, false, true), (1, true, false)],
            Ok(()),
        );
        assert_eq!(
            accounts[4].data().len(),
            transaction_accounts[0].1.data().len(),
        );
        assert_eq!(accounts[2].lamports(), transaction_accounts[2].1.lamports(),);

        // Close program account
        let accounts = process_instruction(
            None,
            &bincode::serialize(&LoaderV4Instruction::SetProgramLength { new_size: 0 }).unwrap(),
            transaction_accounts.clone(),
            &[(0, false, true), (1, true, false), (2, false, true)],
            Ok(()),
        );
        assert_eq!(accounts[0].data().len(), 0);
        assert_eq!(
            accounts[2].lamports(),
            transaction_accounts[2].1.lamports().saturating_add(
                transaction_accounts[0]
                    .1
                    .lamports()
                    .saturating_sub(accounts[0].lamports())
            ),
        );

        // Close uninitialized program account
        process_instruction(
            None,
            &bincode::serialize(&LoaderV4Instruction::SetProgramLength { new_size: 0 }).unwrap(),
            transaction_accounts.clone(),
            &[(3, false, true), (1, true, false), (2, true, true)],
            Ok(()),
        );

        // Error: Program not owned by loader
        process_instruction(
            None,
            &bincode::serialize(&LoaderV4Instruction::SetProgramLength { new_size: 8 }).unwrap(),
            transaction_accounts.clone(),
            &[(1, false, true), (1, true, false), (2, true, true)],
            Err(InstructionError::InvalidAccountOwner),
        );

        // Error: Program is not writeable
        process_instruction(
            None,
            &bincode::serialize(&LoaderV4Instruction::SetProgramLength { new_size: 8 }).unwrap(),
            transaction_accounts.clone(),
            &[(3, false, false), (1, true, false), (2, true, true)],
            Err(InstructionError::InvalidArgument),
        );

        // Error: Close program account without a recipient
        process_instruction(
            None,
            &bincode::serialize(&LoaderV4Instruction::SetProgramLength { new_size: 0 }).unwrap(),
            transaction_accounts.clone(),
            &[(0, false, true), (1, true, false)],
            Err(InstructionError::InvalidArgument),
        );

        // Error: Authority did not sign
        process_instruction(
            None,
            &bincode::serialize(&LoaderV4Instruction::SetProgramLength { new_size: 8 }).unwrap(),
            transaction_accounts.clone(),
            &[(3, true, true), (1, false, false), (2, true, true)],
            Err(InstructionError::MissingRequiredSignature),
        );

        // Error: Program is not retracted
        process_instruction(
            None,
            &bincode::serialize(&LoaderV4Instruction::SetProgramLength { new_size: 8 }).unwrap(),
            transaction_accounts.clone(),
            &[(5, false, true), (1, true, false), (2, false, true)],
            Err(InstructionError::InvalidArgument),
        );

        // Error: Recipient is not writeable
        process_instruction(
            None,
            &bincode::serialize(&LoaderV4Instruction::SetProgramLength { new_size: 0 }).unwrap(),
            transaction_accounts.clone(),
            &[(0, false, true), (1, true, false), (2, false, false)],
            Err(InstructionError::InvalidArgument),
        );

        // Error: Insufficient funds
        process_instruction(
            None,
            &bincode::serialize(&LoaderV4Instruction::SetProgramLength {
                new_size: transaction_accounts[4]
                    .1
                    .data()
                    .len()
                    .saturating_sub(LoaderV4State::program_data_offset())
                    .saturating_add(1) as u32,
            })
            .unwrap(),
            transaction_accounts.clone(),
            &[(0, false, true), (1, true, false)],
            Err(InstructionError::InsufficientFunds),
        );

        test_loader_instruction_general_errors(LoaderV4Instruction::SetProgramLength {
            new_size: 0,
        });
    }

    #[test]
    fn test_loader_instruction_deploy() {
        let authority_address = Pubkey::new_unique();
        let mut transaction_accounts = vec![
            (
                Pubkey::new_unique(),
                load_program_account_from_elf(
                    authority_address,
                    LoaderV4Status::Retracted,
                    "sbpfv3_return_ok",
                ),
            ),
            (
                authority_address,
                AccountSharedData::new(0, 0, &Pubkey::new_unique()),
            ),
            (
                Pubkey::new_unique(),
                load_program_account_from_elf(
                    authority_address,
                    LoaderV4Status::Retracted,
                    "sbpfv3_return_err",
                ),
            ),
            (
                Pubkey::new_unique(),
                AccountSharedData::new(0, 0, &loader_v4::id()),
            ),
            (
                Pubkey::new_unique(),
                load_program_account_from_elf(
                    authority_address,
                    LoaderV4Status::Retracted,
                    "sbpfv0_verifier_err",
                ),
            ),
            (clock::id(), clock(1000)),
            (
                rent::id(),
                create_account_shared_data_for_test(&rent::Rent::default()),
            ),
        ];

        // Deploy from its own data
        let accounts = process_instruction(
            None,
            &bincode::serialize(&LoaderV4Instruction::Deploy).unwrap(),
            transaction_accounts.clone(),
            &[(0, false, true), (1, true, false)],
            Ok(()),
        );
        transaction_accounts[0].1 = accounts[0].clone();
        transaction_accounts[5].1 = clock(2000);
        assert_eq!(
            accounts[0].data().len(),
            transaction_accounts[0].1.data().len(),
        );
        assert_eq!(accounts[0].lamports(), transaction_accounts[0].1.lamports());

        // Error: Program was deployed recently, cooldown still in effect
        process_instruction(
            None,
            &bincode::serialize(&LoaderV4Instruction::Deploy).unwrap(),
            transaction_accounts.clone(),
            &[(0, false, true), (1, true, false)],
            Err(InstructionError::InvalidArgument),
        );
        transaction_accounts[5].1 = clock(3000);

        // Error: Program is uninitialized
        process_instruction(
            None,
            &bincode::serialize(&LoaderV4Instruction::Deploy).unwrap(),
            transaction_accounts.clone(),
            &[(3, false, true), (1, true, false)],
            Err(InstructionError::AccountDataTooSmall),
        );

        // Error: Program fails verification
        process_instruction(
            None,
            &bincode::serialize(&LoaderV4Instruction::Deploy).unwrap(),
            transaction_accounts.clone(),
            &[(4, false, true), (1, true, false)],
            Err(InstructionError::InvalidAccountData),
        );

        // Error: Program is deployed already
        process_instruction(
            None,
            &bincode::serialize(&LoaderV4Instruction::Deploy).unwrap(),
            transaction_accounts.clone(),
            &[(0, false, true), (1, true, false)],
            Err(InstructionError::InvalidArgument),
        );

        test_loader_instruction_general_errors(LoaderV4Instruction::Deploy);
    }

    #[test]
    fn test_loader_instruction_retract() {
        let authority_address = Pubkey::new_unique();
        let mut transaction_accounts = vec![
            (
                Pubkey::new_unique(),
                load_program_account_from_elf(
                    authority_address,
                    LoaderV4Status::Deployed,
                    "sbpfv3_return_err",
                ),
            ),
            (
                authority_address,
                AccountSharedData::new(0, 0, &Pubkey::new_unique()),
            ),
            (
                Pubkey::new_unique(),
                AccountSharedData::new(0, 0, &loader_v4::id()),
            ),
            (
                Pubkey::new_unique(),
                load_program_account_from_elf(
                    authority_address,
                    LoaderV4Status::Retracted,
                    "sbpfv3_return_err",
                ),
            ),
            (clock::id(), clock(1000)),
            (
                rent::id(),
                create_account_shared_data_for_test(&rent::Rent::default()),
            ),
        ];

        // Retract program
        let accounts = process_instruction(
            None,
            &bincode::serialize(&LoaderV4Instruction::Retract).unwrap(),
            transaction_accounts.clone(),
            &[(0, false, true), (1, true, false)],
            Ok(()),
        );
        assert_eq!(
            accounts[0].data().len(),
            transaction_accounts[0].1.data().len(),
        );
        assert_eq!(accounts[0].lamports(), transaction_accounts[0].1.lamports());

        // Error: Program is uninitialized
        process_instruction(
            None,
            &bincode::serialize(&LoaderV4Instruction::Retract).unwrap(),
            transaction_accounts.clone(),
            &[(2, false, true), (1, true, false)],
            Err(InstructionError::AccountDataTooSmall),
        );

        // Error: Program is not deployed
        process_instruction(
            None,
            &bincode::serialize(&LoaderV4Instruction::Retract).unwrap(),
            transaction_accounts.clone(),
            &[(3, false, true), (1, true, false)],
            Err(InstructionError::InvalidArgument),
        );

        // Error: Program was deployed recently, cooldown still in effect
        transaction_accounts[4].1 = clock(0);
        process_instruction(
            None,
            &bincode::serialize(&LoaderV4Instruction::Retract).unwrap(),
            transaction_accounts.clone(),
            &[(0, false, true), (1, true, false)],
            Err(InstructionError::InvalidArgument),
        );

        test_loader_instruction_general_errors(LoaderV4Instruction::Retract);
    }

    #[test]
    fn test_loader_instruction_transfer_authority() {
        let authority_address = Pubkey::new_unique();
        let transaction_accounts = vec![
            (
                Pubkey::new_unique(),
                load_program_account_from_elf(
                    authority_address,
                    LoaderV4Status::Deployed,
                    "sbpfv3_return_err",
                ),
            ),
            (
                Pubkey::new_unique(),
                load_program_account_from_elf(
                    authority_address,
                    LoaderV4Status::Retracted,
                    "sbpfv3_return_err",
                ),
            ),
            (
                Pubkey::new_unique(),
                AccountSharedData::new(0, 0, &loader_v4::id()),
            ),
            (
                authority_address,
                AccountSharedData::new(0, 0, &Pubkey::new_unique()),
            ),
            (
                Pubkey::new_unique(),
                AccountSharedData::new(0, 0, &Pubkey::new_unique()),
            ),
            (
                clock::id(),
                create_account_shared_data_for_test(&clock::Clock::default()),
            ),
            (
                rent::id(),
                create_account_shared_data_for_test(&rent::Rent::default()),
            ),
        ];

        // Transfer authority
        let accounts = process_instruction(
            None,
            &bincode::serialize(&LoaderV4Instruction::TransferAuthority).unwrap(),
            transaction_accounts.clone(),
            &[(0, false, true), (3, true, false), (4, true, false)],
            Ok(()),
        );
        assert_eq!(
            accounts[0].data().len(),
            transaction_accounts[0].1.data().len(),
        );
        assert_eq!(accounts[0].lamports(), transaction_accounts[0].1.lamports());

        // Error: No new authority provided
        process_instruction(
            None,
            &bincode::serialize(&LoaderV4Instruction::TransferAuthority).unwrap(),
            transaction_accounts.clone(),
            &[(0, false, true), (3, true, false)],
            Err(InstructionError::MissingAccount),
        );

        // Error: Program is uninitialized
        process_instruction(
            None,
            &bincode::serialize(&LoaderV4Instruction::TransferAuthority).unwrap(),
            transaction_accounts.clone(),
            &[(2, false, true), (3, true, false), (4, true, false)],
            Err(InstructionError::AccountDataTooSmall),
        );

        // Error: New authority did not sign
        process_instruction(
            None,
            &bincode::serialize(&LoaderV4Instruction::TransferAuthority).unwrap(),
            transaction_accounts.clone(),
            &[(0, false, true), (3, true, false), (4, false, false)],
            Err(InstructionError::MissingRequiredSignature),
        );

        // Error: Authority did not change
        process_instruction(
            None,
            &bincode::serialize(&LoaderV4Instruction::TransferAuthority).unwrap(),
            transaction_accounts,
            &[(0, false, true), (3, true, false), (3, true, false)],
            Err(InstructionError::InvalidArgument),
        );

        test_loader_instruction_general_errors(LoaderV4Instruction::TransferAuthority);
    }

    #[test]
    fn test_loader_instruction_finalize() {
        let authority_address = Pubkey::new_unique();
        let transaction_accounts = vec![
            (
                Pubkey::new_unique(),
                load_program_account_from_elf(
                    authority_address,
                    LoaderV4Status::Deployed,
                    "sbpfv3_return_err",
                ),
            ),
            (
                Pubkey::new_unique(),
                load_program_account_from_elf(
                    authority_address,
                    LoaderV4Status::Retracted,
                    "sbpfv3_return_err",
                ),
            ),
            (
                Pubkey::new_unique(),
                load_program_account_from_elf(
                    authority_address,
                    LoaderV4Status::Finalized,
                    "sbpfv3_return_err",
                ),
            ),
            (
                Pubkey::new_unique(),
                load_program_account_from_elf(
                    Pubkey::new_unique(),
                    LoaderV4Status::Retracted,
                    "sbpfv3_return_err",
                ),
            ),
            (
                Pubkey::new_unique(),
                AccountSharedData::new(0, 0, &loader_v4::id()),
            ),
            (
                authority_address,
                AccountSharedData::new(0, 0, &Pubkey::new_unique()),
            ),
            (
                clock::id(),
                create_account_shared_data_for_test(&clock::Clock::default()),
            ),
            (
                rent::id(),
                create_account_shared_data_for_test(&rent::Rent::default()),
            ),
        ];

        // Finalize program with a next version
        let accounts = process_instruction(
            None,
            &bincode::serialize(&LoaderV4Instruction::Finalize).unwrap(),
            transaction_accounts.clone(),
            &[(0, false, true), (5, true, false), (1, false, false)],
            Ok(()),
        );
        assert_eq!(
            accounts[0].data().len(),
            transaction_accounts[0].1.data().len(),
        );
        assert_eq!(accounts[0].lamports(), transaction_accounts[0].1.lamports());

        // Finalize program with itself as next version
        let accounts = process_instruction(
            None,
            &bincode::serialize(&LoaderV4Instruction::Finalize).unwrap(),
            transaction_accounts.clone(),
            &[(0, false, true), (5, true, false), (0, false, false)],
            Ok(()),
        );
        assert_eq!(
            accounts[0].data().len(),
            transaction_accounts[0].1.data().len(),
        );
        assert_eq!(accounts[0].lamports(), transaction_accounts[0].1.lamports());

        // Error: Program must be deployed to be finalized
        process_instruction(
            None,
            &bincode::serialize(&LoaderV4Instruction::Finalize).unwrap(),
            transaction_accounts.clone(),
            &[(1, false, true), (5, true, false)],
            Err(InstructionError::InvalidArgument),
        );

        // Error: Program is uninitialized
        process_instruction(
            None,
            &bincode::serialize(&LoaderV4Instruction::Finalize).unwrap(),
            transaction_accounts.clone(),
            &[(4, false, true), (5, true, false)],
            Err(InstructionError::AccountDataTooSmall),
        );

        // Error: Next version not owned by loader
        process_instruction(
            None,
            &bincode::serialize(&LoaderV4Instruction::Finalize).unwrap(),
            transaction_accounts.clone(),
            &[(0, false, true), (5, true, false), (5, false, false)],
            Err(InstructionError::InvalidAccountOwner),
        );

        // Error: Program is uninitialized
        process_instruction(
            None,
            &bincode::serialize(&LoaderV4Instruction::Finalize).unwrap(),
            transaction_accounts.clone(),
            &[(0, false, true), (5, true, false), (4, false, false)],
            Err(InstructionError::AccountDataTooSmall),
        );

        // Error: Next version is finalized
        process_instruction(
            None,
            &bincode::serialize(&LoaderV4Instruction::Finalize).unwrap(),
            transaction_accounts.clone(),
            &[(0, false, true), (5, true, false), (2, false, false)],
            Err(InstructionError::Immutable),
        );

        // Error: Incorrect authority of next version
        process_instruction(
            None,
            &bincode::serialize(&LoaderV4Instruction::Finalize).unwrap(),
            transaction_accounts.clone(),
            &[(0, false, true), (5, true, false), (3, false, false)],
            Err(InstructionError::IncorrectAuthority),
        );

        test_loader_instruction_general_errors(LoaderV4Instruction::TransferAuthority);
    }

    #[test]
    fn test_execute_program() {
        let program_address = Pubkey::new_unique();
        let authority_address = Pubkey::new_unique();
        let transaction_accounts = vec![
            (
                program_address,
                load_program_account_from_elf(
                    authority_address,
                    LoaderV4Status::Finalized,
                    "sbpfv3_return_ok",
                ),
            ),
            (
                Pubkey::new_unique(),
                AccountSharedData::new(10000000, 32, &program_address),
            ),
            (
                Pubkey::new_unique(),
                AccountSharedData::new(0, 0, &loader_v4::id()),
            ),
            (
                Pubkey::new_unique(),
                load_program_account_from_elf(
                    authority_address,
                    LoaderV4Status::Retracted,
                    "sbpfv3_return_ok",
                ),
            ),
            (
                Pubkey::new_unique(),
                load_program_account_from_elf(
                    authority_address,
                    LoaderV4Status::Finalized,
                    "sbpfv0_verifier_err",
                ),
            ),
        ];

        // Execute program
        process_instruction(
            Some(0),
            &[0, 1, 2, 3],
            transaction_accounts.clone(),
            &[(1, false, true)],
            Ok(()),
        );

        // Error: Program not owned by loader
        process_instruction(
            Some(1),
            &[0, 1, 2, 3],
            transaction_accounts.clone(),
            &[(1, false, true)],
            Err(InstructionError::UnsupportedProgramId),
        );

        // Error: Program is uninitialized
        process_instruction(
            Some(2),
            &[0, 1, 2, 3],
            transaction_accounts.clone(),
            &[(1, false, true)],
            Err(InstructionError::UnsupportedProgramId),
        );

        // Error: Program is not deployed
        // This is only checked in integration with load_program_accounts() in the SVM
        process_instruction(
            Some(3),
            &[0, 1, 2, 3],
            transaction_accounts.clone(),
            &[(1, false, true)],
            Ok(()),
        );

        // Error: Program fails verification
        process_instruction(
            Some(4),
            &[0, 1, 2, 3],
            transaction_accounts,
            &[(1, false, true)],
            Err(InstructionError::UnsupportedProgramId),
        );
    }
}
