#![cfg(feature = "agave-unstable-api")]
#![deny(clippy::arithmetic_side_effects)]
#![deny(clippy::indexing_slicing)]

#[cfg(feature = "svm-internal")]
use qualifier_attr::qualifiers;
use {
    solana_bincode::limited_deserialize,
    solana_instruction::AccountMeta,
    solana_instruction_error::InstructionError,
    solana_loader_v3_interface::{
        instruction::{MINIMUM_EXTEND_PROGRAM_BYTES, UpgradeableLoaderInstruction},
        state::UpgradeableLoaderState,
    },
    solana_program_runtime::{
        deploy_program,
        invoke_context::InvokeContext,
        program_cache_entry::{ProgramCacheEntry, ProgramCacheEntryOwner, ProgramCacheEntryType},
        sysvar_cache::get_sysvar_with_account_check,
        vm::execute,
    },
    solana_pubkey::Pubkey,
    solana_sbpf::{declare_builtin_function, elf::get_sbpf_version, program::SBPFVersion},
    solana_sdk_ids::{bpf_loader, bpf_loader_deprecated, bpf_loader_upgradeable, native_loader},
    solana_svm_log_collector::{LogCollector, ic_logger_msg},
    solana_svm_measure::measure::Measure,
    solana_svm_type_overrides::sync::Arc,
    solana_system_interface::{MAX_PERMITTED_DATA_LENGTH, instruction as system_instruction},
    solana_transaction_context::{IndexOfAccount, instruction::InstructionContext},
    std::{cell::RefCell, rc::Rc},
    wincode::SchemaRead,
};

#[cfg_attr(feature = "svm-internal", qualifiers(pub))]
const DEFAULT_LOADER_COMPUTE_UNITS: u64 = 570;
#[cfg_attr(feature = "svm-internal", qualifiers(pub))]
const DEPRECATED_LOADER_COMPUTE_UNITS: u64 = 1_140;
#[cfg_attr(feature = "svm-internal", qualifiers(pub))]
const UPGRADEABLE_LOADER_COMPUTE_UNITS: u64 = 2_370;

// `UpgradeableLoaderInstruction::Write`
#[cfg_attr(feature = "svm-internal", qualifiers(pub))]
const WRITE_INSTRUCTION_TAG: [u8; 4] = 1u32.to_le_bytes();

// u32 tag, u32 offset, u64 payload length
#[cfg_attr(feature = "svm-internal", qualifiers(pub))]
const WRITE_INSTRUCTION_HEADER_LEN: usize = 16;

fn parse_write_instruction(instruction_data: &[u8]) -> Result<(u32, &[u8]), InstructionError> {
    #[derive(SchemaRead)]
    struct WriteInstruction<'a> {
        tag: u32,
        offset: u32,
        bytes: &'a [u8],
    }

    let WriteInstruction { tag, offset, bytes } = wincode::deserialize(instruction_data)
        .map_err(|_| InstructionError::InvalidInstructionData)?;

    if tag.to_le_bytes() != WRITE_INSTRUCTION_TAG {
        // unreachable because we only call this in the relevant `match` arm but included for safety
        return Err(InstructionError::InvalidInstructionData);
    }

    if WRITE_INSTRUCTION_HEADER_LEN.saturating_add(bytes.len()) > solana_packet::PACKET_DATA_SIZE {
        return Err(InstructionError::InvalidInstructionData);
    }

    Ok((offset, bytes))
}

fn write_program_data(
    program_data_offset: usize,
    bytes: &[u8],
    instruction_context: &InstructionContext,
    log_collector: &Option<Rc<RefCell<LogCollector>>>,
) -> Result<(), InstructionError> {
    let mut program = instruction_context.try_borrow_instruction_account(0)?;
    let data = program.get_data_mut()?;
    let write_offset = program_data_offset.saturating_add(bytes.len());
    if data.len() < write_offset {
        ic_logger_msg!(
            log_collector,
            "Write overflow: {} < {}",
            data.len(),
            write_offset,
        );
        return Err(InstructionError::AccountDataTooSmall);
    }
    data.get_mut(program_data_offset..write_offset)
        .ok_or(InstructionError::AccountDataTooSmall)?
        .copy_from_slice(bytes);
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
    ) -> Result<u64, Box<dyn std::error::Error>> {
        process_instruction_inner(invoke_context)
    }
);

mod migration_authority {
    solana_pubkey::declare_id!("3Scf35jMNk2xXBD6areNjgMtXgp5ZspDhms8vdcbzC42");
}

#[cfg_attr(feature = "svm-internal", qualifiers(pub))]
pub(crate) fn process_instruction_inner<'a>(
    invoke_context: &mut InvokeContext<'a, 'a>,
) -> Result<u64, Box<dyn std::error::Error>> {
    let log_collector = invoke_context.get_log_collector();
    let transaction_context = &invoke_context.transaction_context;
    let instruction_context = transaction_context.get_current_instruction_context()?;
    let program_id = instruction_context.get_program_key()?;
    let owner_id = instruction_context.get_program_owner()?;

    // Program Management Instruction
    if native_loader::check_id(&owner_id) {
        let program_id = instruction_context.get_program_key()?;
        return if bpf_loader_upgradeable::check_id(program_id) {
            invoke_context.consume_checked(UPGRADEABLE_LOADER_COMPUTE_UNITS)?;
            process_loader_upgradeable_instruction(invoke_context)
        } else if bpf_loader::check_id(program_id) {
            invoke_context.consume_checked(DEFAULT_LOADER_COMPUTE_UNITS)?;
            ic_logger_msg!(
                log_collector,
                "BPF loader management instructions are no longer supported",
            );
            Err(InstructionError::UnsupportedProgramId)
        } else if bpf_loader_deprecated::check_id(program_id) {
            invoke_context.consume_checked(DEPRECATED_LOADER_COMPUTE_UNITS)?;
            ic_logger_msg!(log_collector, "Deprecated loader is no longer supported");
            Err(InstructionError::UnsupportedProgramId)
        } else {
            ic_logger_msg!(log_collector, "Invalid BPF loader id");
            Err(InstructionError::UnsupportedProgramId)
        }
        .map(|_| 0)
        .map_err(|error| Box::new(error) as Box<dyn std::error::Error>);
    }

    // Program Invocation
    let mut get_or_create_executor_time = Measure::start("get_or_create_executor_time");
    let executor = invoke_context
        .program_cache_for_tx_batch
        .find(program_id)
        .ok_or_else(|| {
            ic_logger_msg!(log_collector, "Program is not cached");
            InstructionError::UnsupportedProgramId
        })?;
    get_or_create_executor_time.stop();
    invoke_context.timings.get_or_create_executor_us += get_or_create_executor_time.as_us();

    match &executor.program {
        ProgramCacheEntryType::FailedVerification(_)
        | ProgramCacheEntryType::Closed
        | ProgramCacheEntryType::DelayVisibility => {
            ic_logger_msg!(log_collector, "Program is not deployed");
            Err(Box::new(InstructionError::UnsupportedProgramId) as Box<dyn std::error::Error>)
        }
        ProgramCacheEntryType::Loaded(executable) => execute(executable, invoke_context, &executor),
        _ => Err(Box::new(InstructionError::UnsupportedProgramId) as Box<dyn std::error::Error>),
    }
    .map(|_| 0)
}

fn process_loader_upgradeable_instruction(
    invoke_context: &mut InvokeContext,
) -> Result<(), InstructionError> {
    let log_collector = invoke_context.get_log_collector();
    let transaction_context = &invoke_context.transaction_context;
    let instruction_context = transaction_context.get_current_instruction_context()?;
    let instruction_data = instruction_context.get_instruction_data();
    let program_id = instruction_context.get_program_key()?;

    let instruction = if instruction_data.starts_with(&WRITE_INSTRUCTION_TAG) {
        // `Write` is parsed in-place in the match arm
        UpgradeableLoaderInstruction::Write {
            offset: 0,
            bytes: Vec::new(),
        }
    } else {
        limited_deserialize(instruction_data, solana_packet::PACKET_DATA_SIZE as u64)?
    };

    match instruction {
        UpgradeableLoaderInstruction::InitializeBuffer => {
            instruction_context.check_number_of_instruction_accounts(2)?;
            let mut buffer = instruction_context.try_borrow_instruction_account(0)?;

            if UpgradeableLoaderState::Uninitialized != buffer.get_state()? {
                ic_logger_msg!(log_collector, "Buffer account already initialized");
                return Err(InstructionError::AccountAlreadyInitialized);
            }

            let authority_key = Some(*instruction_context.get_key_of_instruction_account(1)?);

            buffer.set_state(&UpgradeableLoaderState::Buffer {
                authority_address: authority_key,
            })?;
        }
        UpgradeableLoaderInstruction::Write { .. } => {
            let (offset, payload) = parse_write_instruction(instruction_data)?;
            instruction_context.check_number_of_instruction_accounts(2)?;
            let buffer = instruction_context.try_borrow_instruction_account(0)?;

            if let UpgradeableLoaderState::Buffer { authority_address } = buffer.get_state()? {
                if authority_address.is_none() {
                    ic_logger_msg!(log_collector, "Buffer is immutable");
                    return Err(InstructionError::Immutable); // TODO better error code
                }
                let authority_key = Some(*instruction_context.get_key_of_instruction_account(1)?);
                if authority_address != authority_key {
                    ic_logger_msg!(log_collector, "Incorrect buffer authority provided");
                    return Err(InstructionError::IncorrectAuthority);
                }
                if !instruction_context.is_instruction_account_signer(1)? {
                    ic_logger_msg!(log_collector, "Buffer authority did not sign");
                    return Err(InstructionError::MissingRequiredSignature);
                }
            } else {
                ic_logger_msg!(log_collector, "Invalid Buffer account");
                return Err(InstructionError::InvalidAccountData);
            }
            drop(buffer);
            write_program_data(
                UpgradeableLoaderState::size_of_buffer_metadata().saturating_add(offset as usize),
                payload,
                &instruction_context,
                &log_collector,
            )?;
        }
        UpgradeableLoaderInstruction::DeployWithMaxDataLen { max_data_len } => {
            instruction_context.check_number_of_instruction_accounts(4)?;
            let payer_key = *instruction_context.get_key_of_instruction_account(0)?;
            let programdata_key = *instruction_context.get_key_of_instruction_account(1)?;
            let rent =
                get_sysvar_with_account_check::rent(invoke_context, &instruction_context, 4)?;
            let clock =
                get_sysvar_with_account_check::clock(invoke_context, &instruction_context, 5)?;
            instruction_context.check_number_of_instruction_accounts(8)?;
            let authority_key = Some(*instruction_context.get_key_of_instruction_account(7)?);

            // Verify Program account

            let program = instruction_context.try_borrow_instruction_account(2)?;
            if UpgradeableLoaderState::Uninitialized != program.get_state()? {
                ic_logger_msg!(log_collector, "Program account already initialized");
                return Err(InstructionError::AccountAlreadyInitialized);
            }
            if program.get_data().len() < UpgradeableLoaderState::size_of_program() {
                ic_logger_msg!(log_collector, "Program account too small");
                return Err(InstructionError::AccountDataTooSmall);
            }
            if program.get_lamports() < rent.minimum_balance(program.get_data().len()) {
                ic_logger_msg!(log_collector, "Program account not rent-exempt");
                return Err(InstructionError::ExecutableAccountNotRentExempt);
            }
            let new_program_id = *program.get_key();
            drop(program);

            // Verify Buffer account

            let buffer = instruction_context.try_borrow_instruction_account(3)?;
            if !buffer.is_writable() {
                ic_logger_msg!(log_collector, "Buffer account not writeable");
                return Err(InstructionError::InvalidArgument);
            }
            if buffer.get_owner() != program_id {
                ic_logger_msg!(log_collector, "Buffer account not owned by loader");
                return Err(InstructionError::IncorrectProgramId);
            }
            if let UpgradeableLoaderState::Buffer { authority_address } = buffer.get_state()? {
                if authority_address != authority_key {
                    ic_logger_msg!(log_collector, "Buffer and upgrade authority don't match");
                    return Err(InstructionError::IncorrectAuthority);
                }
                if !instruction_context.is_instruction_account_signer(7)? {
                    ic_logger_msg!(log_collector, "Upgrade authority did not sign");
                    return Err(InstructionError::MissingRequiredSignature);
                }
            } else {
                ic_logger_msg!(log_collector, "Invalid Buffer account");
                return Err(InstructionError::InvalidArgument);
            }
            let buffer_key = *buffer.get_key();
            let buffer_data_offset = UpgradeableLoaderState::size_of_buffer_metadata();
            let buffer_data_len = buffer.get_data().len().saturating_sub(buffer_data_offset);
            let programdata_data_offset = UpgradeableLoaderState::size_of_programdata_metadata();
            let programdata_len = UpgradeableLoaderState::size_of_programdata(max_data_len);
            if buffer.get_data().len() < UpgradeableLoaderState::size_of_buffer_metadata()
                || buffer_data_len == 0
            {
                ic_logger_msg!(log_collector, "Buffer account too small");
                return Err(InstructionError::InvalidAccountData);
            }
            drop(buffer);
            if max_data_len < buffer_data_len {
                ic_logger_msg!(
                    log_collector,
                    "Max data length is too small to hold Buffer data"
                );
                return Err(InstructionError::AccountDataTooSmall);
            }
            if programdata_len > MAX_PERMITTED_DATA_LENGTH as usize {
                ic_logger_msg!(log_collector, "Max data length is too large");
                return Err(InstructionError::InvalidArgument);
            }

            // Create ProgramData account
            let (derived_address, bump_seed) =
                Pubkey::find_program_address(&[new_program_id.as_ref()], program_id);
            if derived_address != programdata_key {
                ic_logger_msg!(log_collector, "ProgramData address is not derived");
                return Err(InstructionError::InvalidArgument);
            }

            // Drain the Buffer account to payer before paying for programdata account
            {
                let mut buffer = instruction_context.try_borrow_instruction_account(3)?;
                let mut payer = instruction_context.try_borrow_instruction_account(0)?;
                payer.checked_add_lamports(buffer.get_lamports())?;
                buffer.set_lamports(0)?;
            }

            let owner_id = *program_id;
            let mut instruction = system_instruction::create_account(
                &payer_key,
                &programdata_key,
                1.max(rent.minimum_balance(programdata_len)),
                programdata_len as u64,
                program_id,
            );

            // pass an extra account to avoid the overly strict UnbalancedInstruction error
            instruction
                .accounts
                .push(AccountMeta::new(buffer_key, false));

            invoke_context
                .native_invoke_signed(instruction, &[&[new_program_id.as_ref(), &[bump_seed]]])?;

            // Load and verify the program bits
            let transaction_context = &invoke_context.transaction_context;
            let instruction_context = transaction_context.get_current_instruction_context()?;
            let buffer = instruction_context.try_borrow_instruction_account(3)?;
            deploy_program!(
                invoke_context,
                &new_program_id,
                &owner_id,
                buffer
                    .get_data()
                    .get(buffer_data_offset..)
                    .ok_or(InstructionError::AccountDataTooSmall)?,
                clock.slot,
                invoke_context
                    .get_feature_set()
                    .disable_sbpf_v0_v1_v2_deployment,
            );
            drop(buffer);

            let transaction_context = &invoke_context.transaction_context;
            let instruction_context = transaction_context.get_current_instruction_context()?;

            // Update the ProgramData account and record the program bits
            {
                let mut programdata = instruction_context.try_borrow_instruction_account(1)?;
                programdata.set_state(&UpgradeableLoaderState::ProgramData {
                    slot: clock.slot,
                    upgrade_authority_address: authority_key,
                })?;
                let dst_slice = programdata
                    .get_data_mut()?
                    .get_mut(
                        programdata_data_offset
                            ..programdata_data_offset.saturating_add(buffer_data_len),
                    )
                    .ok_or(InstructionError::AccountDataTooSmall)?;
                let mut buffer = instruction_context.try_borrow_instruction_account(3)?;
                let src_slice = buffer
                    .get_data()
                    .get(buffer_data_offset..)
                    .ok_or(InstructionError::AccountDataTooSmall)?;
                dst_slice.copy_from_slice(src_slice);
                buffer.set_data_length(UpgradeableLoaderState::size_of_buffer(0))?;
            }

            // Update the Program account
            let mut program = instruction_context.try_borrow_instruction_account(2)?;
            program.set_state(&UpgradeableLoaderState::Program {
                programdata_address: programdata_key,
            })?;
            program.set_executable(true)?;
            drop(program);

            ic_logger_msg!(log_collector, "Deployed program {:?}", new_program_id);
        }
        UpgradeableLoaderInstruction::Upgrade => {
            instruction_context.check_number_of_instruction_accounts(3)?;
            let programdata_key = *instruction_context.get_key_of_instruction_account(0)?;
            let rent =
                get_sysvar_with_account_check::rent(invoke_context, &instruction_context, 4)?;
            let clock =
                get_sysvar_with_account_check::clock(invoke_context, &instruction_context, 5)?;
            instruction_context.check_number_of_instruction_accounts(7)?;
            let authority_key = Some(*instruction_context.get_key_of_instruction_account(6)?);

            let set_programdata_to_elf_len = invoke_context
                .get_feature_set()
                .loader_v3_set_program_data_to_elf_length;

            // Verify Program account

            let program = instruction_context.try_borrow_instruction_account(1)?;
            if !program.is_writable() {
                ic_logger_msg!(log_collector, "Program account not writeable");
                return Err(InstructionError::InvalidArgument);
            }
            if program.get_owner() != program_id {
                ic_logger_msg!(log_collector, "Program account not owned by loader");
                return Err(InstructionError::IncorrectProgramId);
            }
            if let UpgradeableLoaderState::Program {
                programdata_address,
            } = program.get_state()?
            {
                if programdata_address != programdata_key {
                    ic_logger_msg!(log_collector, "Program and ProgramData account mismatch");
                    return Err(InstructionError::InvalidArgument);
                }
            } else {
                ic_logger_msg!(log_collector, "Invalid Program account");
                return Err(InstructionError::InvalidAccountData);
            }
            let new_program_id = *program.get_key();
            drop(program);

            // Verify Buffer account

            let buffer = instruction_context.try_borrow_instruction_account(2)?;
            if !buffer.is_writable() {
                ic_logger_msg!(log_collector, "Buffer account not writeable");
                return Err(InstructionError::InvalidArgument);
            }
            if buffer.get_owner() != program_id {
                ic_logger_msg!(log_collector, "Buffer account not owned by loader");
                return Err(InstructionError::IncorrectProgramId);
            }
            if let UpgradeableLoaderState::Buffer { authority_address } = buffer.get_state()? {
                if authority_address != authority_key {
                    ic_logger_msg!(log_collector, "Buffer and upgrade authority don't match");
                    return Err(InstructionError::IncorrectAuthority);
                }
                if !instruction_context.is_instruction_account_signer(6)? {
                    ic_logger_msg!(log_collector, "Upgrade authority did not sign");
                    return Err(InstructionError::MissingRequiredSignature);
                }
            } else {
                ic_logger_msg!(log_collector, "Invalid Buffer account");
                return Err(InstructionError::InvalidArgument);
            }
            let buffer_lamports = buffer.get_lamports();
            let buffer_data_offset = UpgradeableLoaderState::size_of_buffer_metadata();
            let buffer_data_len = buffer.get_data().len().saturating_sub(buffer_data_offset);
            if buffer.get_data().len() < UpgradeableLoaderState::size_of_buffer_metadata()
                || buffer_data_len == 0
            {
                ic_logger_msg!(log_collector, "Buffer account too small");
                return Err(InstructionError::InvalidAccountData);
            }
            drop(buffer);

            // Verify ProgramData account

            let programdata = instruction_context.try_borrow_instruction_account(0)?;
            let programdata_data_offset = UpgradeableLoaderState::size_of_programdata_metadata();
            let (programdata_len, programdata_balance_required) = if set_programdata_to_elf_len {
                // SIMD-0433: we'll resize the programdata account to the new ELF.
                let new_len = programdata_data_offset.saturating_add(buffer_data_len);
                if new_len > MAX_PERMITTED_DATA_LENGTH as usize {
                    ic_logger_msg!(
                        log_collector,
                        "Resized ProgramData length of {} bytes exceeds max account data length",
                        new_len
                    );
                    return Err(InstructionError::InvalidAccountData);
                }
                (new_len, 1.max(rent.minimum_balance(new_len)))
            } else {
                // Before SIMD-0433 accounts must be expanded manually and cannot
                // change size here.
                let len = programdata.get_data().len();
                if len < UpgradeableLoaderState::size_of_programdata(buffer_data_len) {
                    ic_logger_msg!(log_collector, "ProgramData account not large enough");
                    return Err(InstructionError::AccountDataTooSmall);
                }
                (len, 1.max(rent.minimum_balance(len)))
            };
            if programdata.get_lamports().saturating_add(buffer_lamports)
                < programdata_balance_required
            {
                ic_logger_msg!(
                    log_collector,
                    "Buffer account balance too low to fund upgrade"
                );
                return Err(InstructionError::InsufficientFunds);
            }
            if let UpgradeableLoaderState::ProgramData {
                slot,
                upgrade_authority_address,
            } = programdata.get_state()?
            {
                if clock.slot == slot {
                    ic_logger_msg!(log_collector, "Program was deployed in this block already");
                    return Err(InstructionError::InvalidArgument);
                }
                if upgrade_authority_address.is_none() {
                    ic_logger_msg!(log_collector, "Program not upgradeable");
                    return Err(InstructionError::Immutable);
                }
                if upgrade_authority_address != authority_key {
                    ic_logger_msg!(log_collector, "Incorrect upgrade authority provided");
                    return Err(InstructionError::IncorrectAuthority);
                }
                if !instruction_context.is_instruction_account_signer(6)? {
                    ic_logger_msg!(log_collector, "Upgrade authority did not sign");
                    return Err(InstructionError::MissingRequiredSignature);
                }
            } else {
                ic_logger_msg!(log_collector, "Invalid ProgramData account");
                return Err(InstructionError::InvalidAccountData);
            };
            drop(programdata);

            // Load and verify the program bits
            let buffer = instruction_context.try_borrow_instruction_account(2)?;
            deploy_program!(
                invoke_context,
                &new_program_id,
                program_id,
                buffer
                    .get_data()
                    .get(buffer_data_offset..)
                    .ok_or(InstructionError::AccountDataTooSmall)?,
                clock.slot,
                invoke_context
                    .get_feature_set()
                    .disable_sbpf_v0_v1_v2_deployment,
            );
            drop(buffer);

            let transaction_context = &invoke_context.transaction_context;
            let instruction_context = transaction_context.get_current_instruction_context()?;

            // Update the ProgramData account
            let mut programdata = instruction_context.try_borrow_instruction_account(0)?;
            {
                programdata.set_data_length(programdata_len)?;
                programdata.set_state(&UpgradeableLoaderState::ProgramData {
                    slot: clock.slot,
                    upgrade_authority_address: authority_key,
                })?;
                let dst_slice = programdata
                    .get_data_mut()?
                    .get_mut(
                        programdata_data_offset
                            ..programdata_data_offset.saturating_add(buffer_data_len),
                    )
                    .ok_or(InstructionError::AccountDataTooSmall)?;
                let buffer = instruction_context.try_borrow_instruction_account(2)?;
                let src_slice = buffer
                    .get_data()
                    .get(buffer_data_offset..)
                    .ok_or(InstructionError::AccountDataTooSmall)?;
                dst_slice.copy_from_slice(src_slice);
            }
            programdata
                .get_data_mut()?
                .get_mut(programdata_data_offset.saturating_add(buffer_data_len)..)
                .ok_or(InstructionError::AccountDataTooSmall)?
                .fill(0);

            // Fund ProgramData to rent-exemption, spill the rest
            let mut buffer = instruction_context.try_borrow_instruction_account(2)?;
            let mut spill = instruction_context.try_borrow_instruction_account(3)?;
            spill.checked_add_lamports(
                programdata
                    .get_lamports()
                    .saturating_add(buffer_lamports)
                    .saturating_sub(programdata_balance_required),
            )?;
            buffer.set_lamports(0)?;
            programdata.set_lamports(programdata_balance_required)?;
            buffer.set_data_length(UpgradeableLoaderState::size_of_buffer(0))?;

            ic_logger_msg!(log_collector, "Upgraded program {:?}", new_program_id);
        }
        UpgradeableLoaderInstruction::SetAuthority => {
            instruction_context.check_number_of_instruction_accounts(2)?;
            let mut account = instruction_context.try_borrow_instruction_account(0)?;
            let present_authority_key = instruction_context.get_key_of_instruction_account(1)?;
            let new_authority = instruction_context.get_key_of_instruction_account(2).ok();

            match account.get_state()? {
                UpgradeableLoaderState::Buffer { authority_address } => {
                    if new_authority.is_none() {
                        ic_logger_msg!(log_collector, "Buffer authority is not optional");
                        return Err(InstructionError::IncorrectAuthority);
                    }
                    if authority_address.is_none() {
                        ic_logger_msg!(log_collector, "Buffer is immutable");
                        return Err(InstructionError::Immutable);
                    }
                    if authority_address != Some(*present_authority_key) {
                        ic_logger_msg!(log_collector, "Incorrect buffer authority provided");
                        return Err(InstructionError::IncorrectAuthority);
                    }
                    if !instruction_context.is_instruction_account_signer(1)? {
                        ic_logger_msg!(log_collector, "Buffer authority did not sign");
                        return Err(InstructionError::MissingRequiredSignature);
                    }
                    account.set_state(&UpgradeableLoaderState::Buffer {
                        authority_address: new_authority.cloned(),
                    })?;
                }
                UpgradeableLoaderState::ProgramData {
                    slot,
                    upgrade_authority_address,
                } => {
                    if upgrade_authority_address.is_none() {
                        ic_logger_msg!(log_collector, "Program not upgradeable");
                        return Err(InstructionError::Immutable);
                    }
                    if upgrade_authority_address != Some(*present_authority_key) {
                        ic_logger_msg!(log_collector, "Incorrect upgrade authority provided");
                        return Err(InstructionError::IncorrectAuthority);
                    }
                    if !instruction_context.is_instruction_account_signer(1)? {
                        ic_logger_msg!(log_collector, "Upgrade authority did not sign");
                        return Err(InstructionError::MissingRequiredSignature);
                    }
                    if invoke_context
                        .get_feature_set()
                        .disable_sbpf_v0_v1_v2_deployment
                        && new_authority.is_none()
                        && let Some(program) = account
                            .get_data()
                            .get(UpgradeableLoaderState::size_of_programdata_metadata()..)
                        && let Ok(sbpf_version) = get_sbpf_version(program)
                        && sbpf_version < SBPFVersion::V3
                    {
                        return Err(InstructionError::InvalidAccountData);
                    }
                    account.set_state(&UpgradeableLoaderState::ProgramData {
                        slot,
                        upgrade_authority_address: new_authority.cloned(),
                    })?;
                }
                _ => {
                    ic_logger_msg!(log_collector, "Account does not support authorities");
                    return Err(InstructionError::InvalidArgument);
                }
            }

            ic_logger_msg!(log_collector, "New authority {:?}", new_authority);
        }
        UpgradeableLoaderInstruction::SetAuthorityChecked => {
            if !invoke_context
                .get_feature_set()
                .enable_bpf_loader_set_authority_checked_ix
            {
                return Err(InstructionError::InvalidInstructionData);
            }

            instruction_context.check_number_of_instruction_accounts(3)?;
            let mut account = instruction_context.try_borrow_instruction_account(0)?;
            let present_authority_key = instruction_context.get_key_of_instruction_account(1)?;
            let new_authority_key = instruction_context.get_key_of_instruction_account(2)?;

            match account.get_state()? {
                UpgradeableLoaderState::Buffer { authority_address } => {
                    if authority_address.is_none() {
                        ic_logger_msg!(log_collector, "Buffer is immutable");
                        return Err(InstructionError::Immutable);
                    }
                    if authority_address != Some(*present_authority_key) {
                        ic_logger_msg!(log_collector, "Incorrect buffer authority provided");
                        return Err(InstructionError::IncorrectAuthority);
                    }
                    if !instruction_context.is_instruction_account_signer(1)? {
                        ic_logger_msg!(log_collector, "Buffer authority did not sign");
                        return Err(InstructionError::MissingRequiredSignature);
                    }
                    if !instruction_context.is_instruction_account_signer(2)? {
                        ic_logger_msg!(log_collector, "New authority did not sign");
                        return Err(InstructionError::MissingRequiredSignature);
                    }
                    account.set_state(&UpgradeableLoaderState::Buffer {
                        authority_address: Some(*new_authority_key),
                    })?;
                }
                UpgradeableLoaderState::ProgramData {
                    slot,
                    upgrade_authority_address,
                } => {
                    if upgrade_authority_address.is_none() {
                        ic_logger_msg!(log_collector, "Program not upgradeable");
                        return Err(InstructionError::Immutable);
                    }
                    if upgrade_authority_address != Some(*present_authority_key) {
                        ic_logger_msg!(log_collector, "Incorrect upgrade authority provided");
                        return Err(InstructionError::IncorrectAuthority);
                    }
                    if !instruction_context.is_instruction_account_signer(1)? {
                        ic_logger_msg!(log_collector, "Upgrade authority did not sign");
                        return Err(InstructionError::MissingRequiredSignature);
                    }
                    if !instruction_context.is_instruction_account_signer(2)? {
                        ic_logger_msg!(log_collector, "New authority did not sign");
                        return Err(InstructionError::MissingRequiredSignature);
                    }
                    account.set_state(&UpgradeableLoaderState::ProgramData {
                        slot,
                        upgrade_authority_address: Some(*new_authority_key),
                    })?;
                }
                _ => {
                    ic_logger_msg!(log_collector, "Account does not support authorities");
                    return Err(InstructionError::InvalidArgument);
                }
            }

            ic_logger_msg!(log_collector, "New authority {:?}", new_authority_key);
        }
        UpgradeableLoaderInstruction::Close => {
            instruction_context.check_number_of_instruction_accounts(2)?;
            if instruction_context.get_index_of_instruction_account_in_transaction(0)?
                == instruction_context.get_index_of_instruction_account_in_transaction(1)?
            {
                ic_logger_msg!(
                    log_collector,
                    "Recipient is the same as the account being closed"
                );
                return Err(InstructionError::InvalidArgument);
            }
            let mut close_account = instruction_context.try_borrow_instruction_account(0)?;
            let close_key = *close_account.get_key();
            let close_account_state = close_account.get_state()?;
            match close_account_state {
                UpgradeableLoaderState::Uninitialized => {
                    let mut recipient_account =
                        instruction_context.try_borrow_instruction_account(1)?;
                    recipient_account.checked_add_lamports(close_account.get_lamports())?;
                    close_account.set_lamports(0)?;
                    close_account
                        .set_data_length(UpgradeableLoaderState::size_of_uninitialized())?;
                    ic_logger_msg!(log_collector, "Closed Uninitialized {}", close_key);
                }
                UpgradeableLoaderState::Buffer { authority_address } => {
                    instruction_context.check_number_of_instruction_accounts(3)?;
                    drop(close_account);
                    common_close_account(&authority_address, &instruction_context, &log_collector)?;
                    ic_logger_msg!(log_collector, "Closed Buffer {}", close_key);
                }
                UpgradeableLoaderState::ProgramData {
                    slot,
                    upgrade_authority_address: authority_address,
                } => {
                    instruction_context.check_number_of_instruction_accounts(4)?;
                    drop(close_account);
                    let program_account = instruction_context.try_borrow_instruction_account(3)?;
                    let program_key = *program_account.get_key();

                    if !program_account.is_writable() {
                        ic_logger_msg!(log_collector, "Program account is not writable");
                        return Err(InstructionError::InvalidArgument);
                    }
                    if program_account.get_owner() != program_id {
                        ic_logger_msg!(log_collector, "Program account not owned by loader");
                        return Err(InstructionError::IncorrectProgramId);
                    }
                    let clock = invoke_context
                        .environment_config
                        .sysvar_cache()
                        .get_clock()?;
                    if clock.slot == slot {
                        ic_logger_msg!(log_collector, "Program was deployed in this block already");
                        return Err(InstructionError::InvalidArgument);
                    }

                    match program_account.get_state()? {
                        UpgradeableLoaderState::Program {
                            programdata_address,
                        } => {
                            if programdata_address != close_key {
                                ic_logger_msg!(
                                    log_collector,
                                    "ProgramData account does not match ProgramData account"
                                );
                                return Err(InstructionError::InvalidArgument);
                            }

                            drop(program_account);
                            common_close_account(
                                &authority_address,
                                &instruction_context,
                                &log_collector,
                            )?;
                            let clock = invoke_context
                                .environment_config
                                .sysvar_cache()
                                .get_clock()?;
                            invoke_context
                                .program_cache_for_tx_batch
                                .store_modified_entry(
                                    program_key,
                                    Arc::new(ProgramCacheEntry::new_closed_tombstone(
                                        clock.slot,
                                        ProgramCacheEntryOwner::LoaderV3,
                                    )),
                                );
                        }
                        _ => {
                            ic_logger_msg!(log_collector, "Invalid Program account");
                            return Err(InstructionError::InvalidArgument);
                        }
                    }

                    ic_logger_msg!(log_collector, "Closed Program {}", program_key);
                }
                _ => {
                    ic_logger_msg!(log_collector, "Account does not support closing");
                    return Err(InstructionError::InvalidArgument);
                }
            }
        }
        UpgradeableLoaderInstruction::ExtendProgram { additional_bytes } => {
            common_extend_program(invoke_context, additional_bytes, false)?;
        }
    }

    Ok(())
}

fn common_extend_program(
    invoke_context: &mut InvokeContext,
    additional_bytes: u32,
    check_authority: bool,
) -> Result<(), InstructionError> {
    let log_collector = invoke_context.get_log_collector();
    let transaction_context = &invoke_context.transaction_context;
    let instruction_context = transaction_context.get_current_instruction_context()?;
    let program_id = instruction_context.get_program_key()?;

    const PROGRAM_DATA_ACCOUNT_INDEX: IndexOfAccount = 0;
    const PROGRAM_ACCOUNT_INDEX: IndexOfAccount = 1;
    const AUTHORITY_ACCOUNT_INDEX: IndexOfAccount = 2;
    // The unused `system_program_account_index` is 3 if `check_authority` and 2 otherwise.
    let optional_payer_account_index = if check_authority { 4 } else { 3 };

    if additional_bytes == 0 {
        ic_logger_msg!(log_collector, "Additional bytes must be greater than 0");
        return Err(InstructionError::InvalidInstructionData);
    }

    let programdata_account =
        instruction_context.try_borrow_instruction_account(PROGRAM_DATA_ACCOUNT_INDEX)?;
    let programdata_key = *programdata_account.get_key();

    if program_id != programdata_account.get_owner() {
        ic_logger_msg!(log_collector, "ProgramData owner is invalid");
        return Err(InstructionError::InvalidAccountOwner);
    }
    if !programdata_account.is_writable() {
        ic_logger_msg!(log_collector, "ProgramData is not writable");
        return Err(InstructionError::InvalidArgument);
    }

    let program_account =
        instruction_context.try_borrow_instruction_account(PROGRAM_ACCOUNT_INDEX)?;
    if !program_account.is_writable() {
        ic_logger_msg!(log_collector, "Program account is not writable");
        return Err(InstructionError::InvalidArgument);
    }
    if program_account.get_owner() != program_id {
        ic_logger_msg!(log_collector, "Program account not owned by loader");
        return Err(InstructionError::InvalidAccountOwner);
    }
    let program_key = *program_account.get_key();
    match program_account.get_state()? {
        UpgradeableLoaderState::Program {
            programdata_address,
        } => {
            if programdata_address != programdata_key {
                ic_logger_msg!(
                    log_collector,
                    "Program account does not match ProgramData account"
                );
                return Err(InstructionError::InvalidArgument);
            }
        }
        _ => {
            ic_logger_msg!(log_collector, "Invalid Program account");
            return Err(InstructionError::InvalidAccountData);
        }
    }
    drop(program_account);

    let old_len = programdata_account.get_data().len();
    let new_len = old_len.saturating_add(additional_bytes as usize);
    if new_len > MAX_PERMITTED_DATA_LENGTH as usize {
        ic_logger_msg!(
            log_collector,
            "Extended ProgramData length of {} bytes exceeds max account data length of {} bytes",
            new_len,
            MAX_PERMITTED_DATA_LENGTH
        );
        return Err(InstructionError::InvalidRealloc);
    }

    if invoke_context
        .get_feature_set()
        .loader_v3_minimum_extend_program_size
    {
        // SIMD-0431: Minimum Extend Program Size
        //
        // All extensions must be >= 10 KiB in additional_bytes, unless
        // MAX_PERMITTED_DATA_LENGTH - current_len < 10 KiB. In that case,
        // additional_bytes must be equal to the remaining free space.
        let headroom = (MAX_PERMITTED_DATA_LENGTH as usize).saturating_sub(old_len);
        if additional_bytes < MINIMUM_EXTEND_PROGRAM_BYTES
            && (additional_bytes as usize) != headroom
        {
            ic_logger_msg!(
                log_collector,
                "ExtendProgram requires a minimum of {} additional bytes or to extend to maximum \
                 size, but only {} were requested",
                MINIMUM_EXTEND_PROGRAM_BYTES,
                additional_bytes,
            );
            return Err(InstructionError::InvalidArgument);
        }
    }

    let clock_slot = invoke_context
        .environment_config
        .sysvar_cache()
        .get_clock()
        .map(|clock| clock.slot)?;

    let upgrade_authority_address = if let UpgradeableLoaderState::ProgramData {
        slot,
        upgrade_authority_address,
    } = programdata_account.get_state()?
    {
        if clock_slot == slot {
            ic_logger_msg!(log_collector, "Program was extended in this block already");
            return Err(InstructionError::InvalidArgument);
        }

        if upgrade_authority_address.is_none() {
            ic_logger_msg!(
                log_collector,
                "Cannot extend ProgramData accounts that are not upgradeable"
            );
            return Err(InstructionError::Immutable);
        }

        if check_authority {
            let authority_key =
                Some(*instruction_context.get_key_of_instruction_account(AUTHORITY_ACCOUNT_INDEX)?);
            if upgrade_authority_address != authority_key {
                ic_logger_msg!(log_collector, "Incorrect upgrade authority provided");
                return Err(InstructionError::IncorrectAuthority);
            }
            if !instruction_context.is_instruction_account_signer(AUTHORITY_ACCOUNT_INDEX)? {
                ic_logger_msg!(log_collector, "Upgrade authority did not sign");
                return Err(InstructionError::MissingRequiredSignature);
            }
        }

        upgrade_authority_address
    } else {
        ic_logger_msg!(log_collector, "ProgramData state is invalid");
        return Err(InstructionError::InvalidAccountData);
    };

    let required_payment = {
        let balance = programdata_account.get_lamports();
        let rent = invoke_context
            .environment_config
            .sysvar_cache()
            .get_rent()?;
        let min_balance = rent.minimum_balance(new_len).max(1);
        min_balance.saturating_sub(balance)
    };

    // Borrowed accounts need to be dropped before native_invoke_signed
    drop(programdata_account);

    // Dereference the program ID to prevent overlapping mutable/immutable borrow of invoke context
    let program_id = *program_id;
    if required_payment > 0 {
        let payer_key =
            *instruction_context.get_key_of_instruction_account(optional_payer_account_index)?;

        invoke_context.native_invoke_signed(
            system_instruction::transfer(&payer_key, &programdata_key, required_payment),
            &[],
        )?;
    }

    let transaction_context = &invoke_context.transaction_context;
    let instruction_context = transaction_context.get_current_instruction_context()?;
    let mut programdata_account =
        instruction_context.try_borrow_instruction_account(PROGRAM_DATA_ACCOUNT_INDEX)?;
    programdata_account.set_data_length(new_len)?;

    let programdata_data_offset = UpgradeableLoaderState::size_of_programdata_metadata();

    deploy_program!(
        invoke_context,
        &program_key,
        &program_id,
        programdata_account
            .get_data()
            .get(programdata_data_offset..)
            .ok_or(InstructionError::AccountDataTooSmall)?,
        clock_slot,
        false, // disable_sbpf_v0_v1_v2_deployment // explicitly continue to allow them for extend program
    );
    drop(programdata_account);

    let mut programdata_account =
        instruction_context.try_borrow_instruction_account(PROGRAM_DATA_ACCOUNT_INDEX)?;
    programdata_account.set_state(&UpgradeableLoaderState::ProgramData {
        slot: clock_slot,
        upgrade_authority_address,
    })?;

    ic_logger_msg!(
        log_collector,
        "Extended ProgramData account by {} bytes",
        additional_bytes
    );

    Ok(())
}

fn common_close_account(
    authority_address: &Option<Pubkey>,
    instruction_context: &InstructionContext,
    log_collector: &Option<Rc<RefCell<LogCollector>>>,
) -> Result<(), InstructionError> {
    if authority_address.is_none() {
        ic_logger_msg!(log_collector, "Account is immutable");
        return Err(InstructionError::Immutable);
    }
    if *authority_address != Some(*instruction_context.get_key_of_instruction_account(2)?) {
        ic_logger_msg!(log_collector, "Incorrect authority provided");
        return Err(InstructionError::IncorrectAuthority);
    }
    if !instruction_context.is_instruction_account_signer(2)? {
        ic_logger_msg!(log_collector, "Authority did not sign");
        return Err(InstructionError::MissingRequiredSignature);
    }

    let mut close_account = instruction_context.try_borrow_instruction_account(0)?;
    let mut recipient_account = instruction_context.try_borrow_instruction_account(1)?;

    recipient_account.checked_add_lamports(close_account.get_lamports())?;
    close_account.set_lamports(0)?;
    close_account.set_data_length(UpgradeableLoaderState::size_of_uninitialized())?;
    close_account.set_state(&UpgradeableLoaderState::Uninitialized)?;
    Ok(())
}

#[cfg_attr(feature = "svm-internal", qualifiers(pub))]
mod test_utils {
    #[cfg(all(feature = "svm-internal", feature = "metrics"))]
    use solana_program_runtime::program_metrics::LoadProgramMetrics;
    #[cfg(feature = "svm-internal")]
    use {
        super::*, solana_account::ReadableAccount,
        solana_program_runtime::loaded_programs::ProgramRuntimeEnvironment,
        solana_syscalls::create_program_runtime_environment,
    };

    #[cfg(feature = "svm-internal")]
    fn check_loader_id(id: &Pubkey) -> bool {
        bpf_loader::check_id(id)
            || bpf_loader_deprecated::check_id(id)
            || bpf_loader_upgradeable::check_id(id)
    }

    #[cfg(feature = "svm-internal")]
    #[cfg_attr(feature = "svm-internal", qualifiers(pub))]
    fn load_all_invoked_programs(invoke_context: &mut InvokeContext) {
        let program_runtime_environment = create_program_runtime_environment(
            invoke_context.get_feature_set(),
            invoke_context.get_compute_budget(),
            false, /* deployment */
            false, /* debugging_features */
        )
        .unwrap();
        let num_accounts = invoke_context.transaction_context.get_number_of_accounts();
        for index in 0..num_accounts {
            let account = invoke_context
                .transaction_context
                .accounts()
                .try_borrow(index)
                .expect("Failed to get the account");

            let owner = account.owner();
            if check_loader_id(owner) {
                let programdata_data_offset = 0;
                let pubkey = invoke_context
                    .transaction_context
                    .get_key_of_account_at_index(index)
                    .expect("Failed to get account key");

                let programdata = account
                    .data()
                    .get(programdata_data_offset.min(account.data().len())..)
                    .unwrap();
                let loaded_program = ProgramCacheEntry::load(
                    owner,
                    ProgramRuntimeEnvironment::clone(&program_runtime_environment),
                    0,
                    programdata,
                    #[cfg(feature = "metrics")]
                    &mut LoadProgramMetrics::default(),
                )
                .map_err(|_| InstructionError::InvalidAccountData);
                if let Ok(loaded_program) = loaded_program {
                    invoke_context
                        .program_cache_for_tx_batch
                        .store_modified_entry(*pubkey, Arc::new(loaded_program));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        assert_matches::assert_matches,
        solana_account::{AccountSharedData, WritableAccount},
        solana_epoch_schedule::EpochSchedule,
        solana_instruction_error::InstructionError,
        solana_program_runtime::{
            loaded_programs::ProgramRuntimeEnvironment, program_metrics::ProgramStatistics,
            vm::calculate_heap_cost, with_mock_invoke_context,
        },
        solana_pubkey::Pubkey,
        solana_sbpf::program::BuiltinProgram,
        solana_sdk_ids::sysvar,
        solana_svm_type_overrides::sync::atomic::{AtomicU64, Ordering},
        solana_sysvar_id::SysvarId,
        std::{fs::File, io::Read},
    };

    fn create_sysvar_account<T>(value: &T) -> AccountSharedData
    where
        T: wincode::Serialize<Src = T> + SysvarId,
    {
        let serialized_len = wincode::serialized_size(value).unwrap() as usize;
        let canonical_data_len = match T::id() {
            sysvar::clock::ID => solana_clock::SIZE,
            sysvar::epoch_schedule::ID => solana_epoch_schedule::SIZE,
            sysvar::rent::ID => solana_rent::SIZE,
            id => panic!("unsupported sysvar: {id}"),
        };
        let required_data_len = canonical_data_len.max(serialized_len);
        let mut account = AccountSharedData::new(1, required_data_len, &sysvar::id());
        wincode::serialize_into(account.data_as_mut_slice(), value).unwrap();
        account
    }

    #[test]
    fn test_write_instruction_matches_sdk() {
        let instruction = UpgradeableLoaderInstruction::Write {
            offset: 7,
            bytes: vec![1, 2, 3],
        };
        // If this pattern fails, it means UpgradeableLoaderInstruction::Write has been changed
        // in the SDK. WriteInstruction in parse_write_instruction() MUST be changed to match it.
        let UpgradeableLoaderInstruction::Write { offset, bytes } = &instruction else {
            unreachable!()
        };
        let serialized = bincode::serialize(&instruction).unwrap();
        assert!(serialized.starts_with(&WRITE_INSTRUCTION_TAG));
        assert_eq!(
            parse_write_instruction(&serialized).unwrap(),
            (*offset, bytes.as_slice())
        );
    }

    #[test]
    fn test_calculate_heap_cost() {
        let heap_cost = 8_u64;

        // heap allocations are in 32K block, `heap_cost` of CU is consumed per additional 32k

        // assert less than 32K heap should cost zero unit
        assert_eq!(0, calculate_heap_cost(31 * 1024, heap_cost));

        // assert exact 32K heap should be cost zero unit
        assert_eq!(0, calculate_heap_cost(32 * 1024, heap_cost));

        // assert slightly more than 32K heap should cost 1 * heap_cost
        assert_eq!(heap_cost, calculate_heap_cost(33 * 1024, heap_cost));

        // assert exact 64K heap should cost 1 * heap_cost
        assert_eq!(heap_cost, calculate_heap_cost(64 * 1024, heap_cost));
    }

    fn deploy_test_program(
        invoke_context: &mut InvokeContext,
        program_id: Pubkey,
    ) -> Result<(), InstructionError> {
        let mut file = File::open("test_elfs/out/sbpfv3_return_ok.so").expect("file open failed");
        let mut elf = Vec::new();
        file.read_to_end(&mut elf).unwrap();
        deploy_program!(
            invoke_context,
            &program_id,
            &bpf_loader_upgradeable::id(),
            &elf,
            2_u64,
            true, // disable_sbpf_v0_v1_v2_deployment
        );
        Ok(())
    }

    // Concurrency rationale: these tests construct `ProgramCacheEntry` instances
    // directly. The struct's `latest_access_slot: AtomicU64` field is defined in
    // `solana-program-runtime`; under the `shuttle-test` feature
    // `solana-svm-type-overrides` swaps `std::sync::atomic::AtomicU64` for
    // `shuttle::sync::atomic::AtomicU64`, whose Shuttle-backed operations
    // (load, fetch_max, and similar) must run inside an active Shuttle
    // scheduler. We therefore extract the test bodies into `do_test_*` helpers
    // and drive them via `shuttle::check_random` stubs when the feature is on.
    // We use `check_random` only (no `check_dfs` companion) because the test
    // bodies spawn no Shuttle threads, so DFS gives no meaningful interleaving
    // coverage; `check_random` is enough to provide the scheduler context.
    // This matches the single-scheduler pattern used in
    // `net-utils/src/token_bucket.rs` and `poh/src/record_channels.rs`.
    //
    // 100 iterations is intentionally low: the test bodies are single-threaded
    // (no `shuttle::thread::spawn`), so additional iterations validate only
    // the harness wiring, not concurrent interleavings. Bump this if a future
    // refactor introduces real concurrency in the test bodies.
    #[cfg(feature = "shuttle-test")]
    const PROGRAM_USAGE_COUNT_RANDOM_ITERATIONS: usize = 100;

    #[test]
    fn test_program_usage_count_on_upgrade() {
        #[cfg(feature = "shuttle-test")]
        shuttle::check_random(
            do_test_program_usage_count_on_upgrade,
            PROGRAM_USAGE_COUNT_RANDOM_ITERATIONS,
        );
        #[cfg(not(feature = "shuttle-test"))]
        do_test_program_usage_count_on_upgrade();
    }

    fn do_test_program_usage_count_on_upgrade() {
        let transaction_accounts = vec![(
            sysvar::epoch_schedule::id(),
            create_sysvar_account(&EpochSchedule::default()),
        )];
        with_mock_invoke_context!(invoke_context, transaction_context, transaction_accounts);
        let program_id = Pubkey::new_unique();
        let env = ProgramRuntimeEnvironment::from(BuiltinProgram::new_mock());
        let stats = ProgramStatistics {
            uses: 100.into(),
            ..Default::default()
        };
        let program = ProgramCacheEntry {
            program: ProgramCacheEntryType::Unloaded(env),
            account_owner: ProgramCacheEntryOwner::LoaderV2,
            deployment_slot: 0,
            stats: stats.into(),
            latest_access_slot: AtomicU64::new(0),
        };
        invoke_context
            .program_cache_for_tx_batch
            .replenish(program_id, Arc::new(program));
        invoke_context
            .program_cache_for_tx_batch
            .set_slot_for_tests(2);

        assert_matches!(
            deploy_test_program(&mut invoke_context, program_id,),
            Ok(())
        );

        let updated_program = invoke_context
            .program_cache_for_tx_batch
            .find(&program_id)
            .expect("Didn't find upgraded program in the cache");

        assert_eq!(updated_program.deployment_slot, 2);
        assert_eq!(updated_program.stats.uses.load(Ordering::Relaxed), 100);
    }

    #[test]
    fn test_program_usage_count_on_non_upgrade() {
        #[cfg(feature = "shuttle-test")]
        shuttle::check_random(
            do_test_program_usage_count_on_non_upgrade,
            PROGRAM_USAGE_COUNT_RANDOM_ITERATIONS,
        );
        #[cfg(not(feature = "shuttle-test"))]
        do_test_program_usage_count_on_non_upgrade();
    }

    fn do_test_program_usage_count_on_non_upgrade() {
        let transaction_accounts = vec![(
            sysvar::epoch_schedule::id(),
            create_sysvar_account(&EpochSchedule::default()),
        )];
        with_mock_invoke_context!(invoke_context, transaction_context, transaction_accounts);
        let program_id = Pubkey::new_unique();
        let env = ProgramRuntimeEnvironment::from(BuiltinProgram::new_mock());
        let stats = ProgramStatistics {
            uses: 100.into(),
            ..Default::default()
        };
        let program = ProgramCacheEntry {
            program: ProgramCacheEntryType::Unloaded(env),
            account_owner: ProgramCacheEntryOwner::LoaderV2,
            deployment_slot: 0,
            stats: stats.into(),
            latest_access_slot: AtomicU64::new(0),
        };
        invoke_context
            .program_cache_for_tx_batch
            .replenish(program_id, Arc::new(program));
        invoke_context
            .program_cache_for_tx_batch
            .set_slot_for_tests(2);

        let program_id2 = Pubkey::new_unique();
        assert_matches!(
            deploy_test_program(&mut invoke_context, program_id2),
            Ok(())
        );

        let program2 = invoke_context
            .program_cache_for_tx_batch
            .find(&program_id2)
            .expect("Didn't find upgraded program in the cache");

        assert_eq!(program2.deployment_slot, 2);
        assert_eq!(program2.stats.uses.load(Ordering::Relaxed), 0);
    }
}
