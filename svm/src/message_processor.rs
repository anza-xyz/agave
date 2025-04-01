use {
    agave_precompiles::get_precompile,
    solana_account::WritableAccount,
    solana_instructions_sysvar as instructions,
    solana_measure::measure_us,
    solana_program_runtime::invoke_context::InvokeContext,
    solana_svm_transaction::svm_message::SVMMessage,
    solana_timings::{ExecuteDetailsTimings, ExecuteTimings},
    solana_transaction_context::{IndexOfAccount, InstructionAccount},
    solana_transaction_error::TransactionError,
};

/// Process a message.
/// This method calls each instruction in the message over the set of loaded accounts.
/// For each instruction it calls the program entrypoint method and verifies that the result of
/// the call does not violate the bank's accounting rules.
/// The accounts are committed back to the bank only if every instruction succeeds.
pub(crate) fn process_message(
    message: &impl SVMMessage,
    program_indices: &[Vec<IndexOfAccount>],
    invoke_context: &mut InvokeContext,
    execute_timings: &mut ExecuteTimings,
    accumulated_consumed_units: &mut u64,
) -> Result<(), TransactionError> {
    debug_assert_eq!(program_indices.len(), message.num_instructions());
    for (instruction_index, ((program_id, instruction), program_indices)) in message
        .program_instructions_iter()
        .zip(program_indices.iter())
        .enumerate()
    {
        // Fixup the special instructions key if present
        // before the account pre-values are taken care of
        if let Some(account_index) = invoke_context
            .transaction_context
            .find_index_of_account(&instructions::id())
        {
            let mut mut_account_ref = invoke_context
                .transaction_context
                .accounts()
                .try_borrow_mut(account_index)
                .map_err(|_| TransactionError::InvalidAccountIndex)?;
            instructions::store_current_index(
                mut_account_ref.data_as_mut_slice(),
                instruction_index as u16,
            );
        }

        let mut instruction_accounts = Vec::with_capacity(instruction.accounts.len());
        for (instruction_account_index, index_in_transaction) in
            instruction.accounts.iter().enumerate()
        {
            let index_in_callee = instruction
                .accounts
                .get(0..instruction_account_index)
                .ok_or(TransactionError::InvalidAccountIndex)?
                .iter()
                .position(|account_index| account_index == index_in_transaction)
                .unwrap_or(instruction_account_index)
                as IndexOfAccount;
            let index_in_transaction = *index_in_transaction as usize;
            instruction_accounts.push(InstructionAccount {
                index_in_transaction: index_in_transaction as IndexOfAccount,
                index_in_caller: index_in_transaction as IndexOfAccount,
                index_in_callee,
                is_signer: message.is_signer(index_in_transaction),
                is_writable: message.is_writable(index_in_transaction),
            });
        }

        let mut compute_units_consumed = 0;
        let (result, process_instruction_us) = measure_us!({
            if let Some(precompile) = get_precompile(program_id, |feature_id| {
                invoke_context.get_feature_set().is_active(feature_id)
            }) {
                invoke_context.process_precompile(
                    precompile,
                    instruction.data,
                    &instruction_accounts,
                    program_indices,
                    message.instructions_iter().map(|ix| ix.data),
                )
            } else {
                invoke_context.process_instruction(
                    instruction.data,
                    &instruction_accounts,
                    program_indices,
                    &mut compute_units_consumed,
                    execute_timings,
                )
            }
        });

        *accumulated_consumed_units =
            accumulated_consumed_units.saturating_add(compute_units_consumed);
        execute_timings.details.accumulate_program(
            program_id,
            process_instruction_us,
            compute_units_consumed,
            result.is_err(),
        );
        invoke_context.timings = {
            execute_timings.details.accumulate(&invoke_context.timings);
            ExecuteDetailsTimings::default()
        };
        execute_timings
            .execute_accessories
            .process_instructions
            .total_us += process_instruction_us;

        result.map_err(|err| TransactionError::InstructionError(instruction_index as u8, err))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        agave_feature_set::FeatureSet,
        agave_reserved_account_keys::ReservedAccountKeys,
        openssl::{
            ec::{EcGroup, EcKey},
            nid::Nid,
        },
        rand0_7::thread_rng,
        solana_account::{
            Account, AccountSharedData, ReadableAccount, DUMMY_INHERITABLE_ACCOUNT_FIELDS,
        },
        solana_ed25519_program::new_ed25519_instruction,
        solana_hash::Hash,
        solana_instruction::{error::InstructionError, AccountMeta, Instruction},
        solana_message::{AccountKeys, Message, SanitizedMessage},
        solana_program_runtime::{
            declare_process_instruction,
            execution_budget::{SVMTransactionExecutionBudget, SVMTransactionExecutionCost},
            invoke_context::EnvironmentConfig,
            loaded_programs::{ProgramCacheEntry, ProgramCacheForTxBatch},
            sysvar_cache::SysvarCache,
        },
        solana_pubkey::Pubkey,
        solana_rent::Rent,
        solana_sdk_ids::{ed25519_program, native_loader, secp256k1_program, system_program},
        solana_secp256k1_program::new_secp256k1_instruction,
        solana_secp256r1_program::new_secp256r1_instruction,
        solana_svm_callback::EpochStakeCallback,
        solana_transaction_context::TransactionContext,
        std::sync::Arc,
    };

    struct MockCallback {}
    impl EpochStakeCallback for MockCallback {}

    fn create_loadable_account_for_test(name: &str) -> AccountSharedData {
        let (lamports, rent_epoch) = DUMMY_INHERITABLE_ACCOUNT_FIELDS;
        AccountSharedData::from(Account {
            lamports,
            owner: native_loader::id(),
            data: name.as_bytes().to_vec(),
            executable: true,
            rent_epoch,
        })
    }

    fn new_sanitized_message(message: Message) -> SanitizedMessage {
        SanitizedMessage::try_from_legacy_message(message, &ReservedAccountKeys::empty_key_set())
            .unwrap()
    }

    #[test]
    fn test_process_message_readonly_handling() {
        #[derive(serde_derive::Serialize, serde_derive::Deserialize)]
        enum MockSystemInstruction {
            Correct,
            TransferLamports { lamports: u64 },
            ChangeData { data: u8 },
        }

        declare_process_instruction!(MockBuiltin, 1, |invoke_context| {
            let transaction_context = &invoke_context.transaction_context;
            let instruction_context = transaction_context.get_current_instruction_context()?;
            let instruction_data = instruction_context.get_instruction_data();
            if let Ok(instruction) = bincode::deserialize(instruction_data) {
                match instruction {
                    MockSystemInstruction::Correct => Ok(()),
                    MockSystemInstruction::TransferLamports { lamports } => {
                        instruction_context
                            .try_borrow_instruction_account(transaction_context, 0)?
                            .checked_sub_lamports(lamports)?;
                        instruction_context
                            .try_borrow_instruction_account(transaction_context, 1)?
                            .checked_add_lamports(lamports)?;
                        Ok(())
                    }
                    MockSystemInstruction::ChangeData { data } => {
                        instruction_context
                            .try_borrow_instruction_account(transaction_context, 1)?
                            .set_data(vec![data])?;
                        Ok(())
                    }
                }
            } else {
                Err(InstructionError::InvalidInstructionData)
            }
        });

        let writable_pubkey = Pubkey::new_unique();
        let readonly_pubkey = Pubkey::new_unique();
        let mock_system_program_id = Pubkey::new_unique();

        let accounts = vec![
            (
                writable_pubkey,
                AccountSharedData::new(100, 1, &mock_system_program_id),
            ),
            (
                readonly_pubkey,
                AccountSharedData::new(0, 1, &mock_system_program_id),
            ),
            (
                mock_system_program_id,
                create_loadable_account_for_test("mock_system_program"),
            ),
        ];
        let mut transaction_context = TransactionContext::new(accounts, Rent::default(), 1, 3);
        let program_indices = vec![vec![2]];
        let mut program_cache_for_tx_batch = ProgramCacheForTxBatch::default();
        program_cache_for_tx_batch.replenish(
            mock_system_program_id,
            Arc::new(ProgramCacheEntry::new_builtin(0, 0, MockBuiltin::vm)),
        );
        let account_keys = (0..transaction_context.get_number_of_accounts())
            .map(|index| {
                *transaction_context
                    .get_key_of_account_at_index(index)
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let account_metas = vec![
            AccountMeta::new(writable_pubkey, true),
            AccountMeta::new_readonly(readonly_pubkey, false),
        ];

        let message = new_sanitized_message(Message::new_with_compiled_instructions(
            1,
            0,
            2,
            account_keys.clone(),
            Hash::default(),
            AccountKeys::new(&account_keys, None).compile_instructions(&[
                Instruction::new_with_bincode(
                    mock_system_program_id,
                    &MockSystemInstruction::Correct,
                    account_metas.clone(),
                ),
            ]),
        ));
        let sysvar_cache = SysvarCache::default();
        let environment_config = EnvironmentConfig::new(
            Hash::default(),
            0,
            &MockCallback {},
            Arc::new(FeatureSet::all_enabled()),
            &sysvar_cache,
        );
        let mut invoke_context = InvokeContext::new(
            &mut transaction_context,
            &mut program_cache_for_tx_batch,
            environment_config,
            None,
            SVMTransactionExecutionBudget::default(),
            SVMTransactionExecutionCost::default(),
        );
        let result = process_message(
            &message,
            &program_indices,
            &mut invoke_context,
            &mut ExecuteTimings::default(),
            &mut 0,
        );
        assert!(result.is_ok());
        assert_eq!(
            transaction_context
                .accounts()
                .try_borrow(0)
                .unwrap()
                .lamports(),
            100
        );
        assert_eq!(
            transaction_context
                .accounts()
                .try_borrow(1)
                .unwrap()
                .lamports(),
            0
        );

        let message = new_sanitized_message(Message::new_with_compiled_instructions(
            1,
            0,
            2,
            account_keys.clone(),
            Hash::default(),
            AccountKeys::new(&account_keys, None).compile_instructions(&[
                Instruction::new_with_bincode(
                    mock_system_program_id,
                    &MockSystemInstruction::TransferLamports { lamports: 50 },
                    account_metas.clone(),
                ),
            ]),
        ));
        let environment_config = EnvironmentConfig::new(
            Hash::default(),
            0,
            &MockCallback {},
            Arc::new(FeatureSet::all_enabled()),
            &sysvar_cache,
        );
        let mut invoke_context = InvokeContext::new(
            &mut transaction_context,
            &mut program_cache_for_tx_batch,
            environment_config,
            None,
            SVMTransactionExecutionBudget::default(),
            SVMTransactionExecutionCost::default(),
        );
        let result = process_message(
            &message,
            &program_indices,
            &mut invoke_context,
            &mut ExecuteTimings::default(),
            &mut 0,
        );
        assert_eq!(
            result,
            Err(TransactionError::InstructionError(
                0,
                InstructionError::ReadonlyLamportChange
            ))
        );

        let message = new_sanitized_message(Message::new_with_compiled_instructions(
            1,
            0,
            2,
            account_keys.clone(),
            Hash::default(),
            AccountKeys::new(&account_keys, None).compile_instructions(&[
                Instruction::new_with_bincode(
                    mock_system_program_id,
                    &MockSystemInstruction::ChangeData { data: 50 },
                    account_metas,
                ),
            ]),
        ));
        let environment_config = EnvironmentConfig::new(
            Hash::default(),
            0,
            &MockCallback {},
            Arc::new(FeatureSet::all_enabled()),
            &sysvar_cache,
        );
        let mut invoke_context = InvokeContext::new(
            &mut transaction_context,
            &mut program_cache_for_tx_batch,
            environment_config,
            None,
            SVMTransactionExecutionBudget::default(),
            SVMTransactionExecutionCost::default(),
        );
        let result = process_message(
            &message,
            &program_indices,
            &mut invoke_context,
            &mut ExecuteTimings::default(),
            &mut 0,
        );
        assert_eq!(
            result,
            Err(TransactionError::InstructionError(
                0,
                InstructionError::ReadonlyDataModified
            ))
        );
    }

    #[test]
    fn test_process_message_duplicate_accounts() {
        #[derive(serde_derive::Serialize, serde_derive::Deserialize)]
        enum MockSystemInstruction {
            BorrowFail,
            MultiBorrowMut,
            DoWork { lamports: u64, data: u8 },
        }

        declare_process_instruction!(MockBuiltin, 1, |invoke_context| {
            let transaction_context = &invoke_context.transaction_context;
            let instruction_context = transaction_context.get_current_instruction_context()?;
            let instruction_data = instruction_context.get_instruction_data();
            let mut to_account =
                instruction_context.try_borrow_instruction_account(transaction_context, 1)?;
            if let Ok(instruction) = bincode::deserialize(instruction_data) {
                match instruction {
                    MockSystemInstruction::BorrowFail => {
                        let from_account = instruction_context
                            .try_borrow_instruction_account(transaction_context, 0)?;
                        let dup_account = instruction_context
                            .try_borrow_instruction_account(transaction_context, 2)?;
                        if from_account.get_lamports() != dup_account.get_lamports() {
                            return Err(InstructionError::InvalidArgument);
                        }
                        Ok(())
                    }
                    MockSystemInstruction::MultiBorrowMut => {
                        let lamports_a = instruction_context
                            .try_borrow_instruction_account(transaction_context, 0)?
                            .get_lamports();
                        let lamports_b = instruction_context
                            .try_borrow_instruction_account(transaction_context, 2)?
                            .get_lamports();
                        if lamports_a != lamports_b {
                            return Err(InstructionError::InvalidArgument);
                        }
                        Ok(())
                    }
                    MockSystemInstruction::DoWork { lamports, data } => {
                        let mut dup_account = instruction_context
                            .try_borrow_instruction_account(transaction_context, 2)?;
                        dup_account.checked_sub_lamports(lamports)?;
                        to_account.checked_add_lamports(lamports)?;
                        dup_account.set_data(vec![data])?;
                        drop(dup_account);
                        let mut from_account = instruction_context
                            .try_borrow_instruction_account(transaction_context, 0)?;
                        from_account.checked_sub_lamports(lamports)?;
                        to_account.checked_add_lamports(lamports)?;
                        Ok(())
                    }
                }
            } else {
                Err(InstructionError::InvalidInstructionData)
            }
        });
        let mock_program_id = Pubkey::from([2u8; 32]);
        let accounts = vec![
            (
                solana_pubkey::new_rand(),
                AccountSharedData::new(100, 1, &mock_program_id),
            ),
            (
                solana_pubkey::new_rand(),
                AccountSharedData::new(0, 1, &mock_program_id),
            ),
            (
                mock_program_id,
                create_loadable_account_for_test("mock_system_program"),
            ),
        ];
        let mut transaction_context = TransactionContext::new(accounts, Rent::default(), 1, 3);
        let program_indices = vec![vec![2]];
        let mut program_cache_for_tx_batch = ProgramCacheForTxBatch::default();
        program_cache_for_tx_batch.replenish(
            mock_program_id,
            Arc::new(ProgramCacheEntry::new_builtin(0, 0, MockBuiltin::vm)),
        );
        let account_metas = vec![
            AccountMeta::new(
                *transaction_context.get_key_of_account_at_index(0).unwrap(),
                true,
            ),
            AccountMeta::new(
                *transaction_context.get_key_of_account_at_index(1).unwrap(),
                false,
            ),
            AccountMeta::new(
                *transaction_context.get_key_of_account_at_index(0).unwrap(),
                false,
            ),
        ];

        // Try to borrow mut the same account
        let message = new_sanitized_message(Message::new(
            &[Instruction::new_with_bincode(
                mock_program_id,
                &MockSystemInstruction::BorrowFail,
                account_metas.clone(),
            )],
            Some(transaction_context.get_key_of_account_at_index(0).unwrap()),
        ));
        let sysvar_cache = SysvarCache::default();
        let environment_config = EnvironmentConfig::new(
            Hash::default(),
            0,
            &MockCallback {},
            Arc::new(FeatureSet::all_enabled()),
            &sysvar_cache,
        );
        let mut invoke_context = InvokeContext::new(
            &mut transaction_context,
            &mut program_cache_for_tx_batch,
            environment_config,
            None,
            SVMTransactionExecutionBudget::default(),
            SVMTransactionExecutionCost::default(),
        );
        let result = process_message(
            &message,
            &program_indices,
            &mut invoke_context,
            &mut ExecuteTimings::default(),
            &mut 0,
        );
        assert_eq!(
            result,
            Err(TransactionError::InstructionError(
                0,
                InstructionError::AccountBorrowFailed
            ))
        );

        // Try to borrow mut the same account in a safe way
        let message = new_sanitized_message(Message::new(
            &[Instruction::new_with_bincode(
                mock_program_id,
                &MockSystemInstruction::MultiBorrowMut,
                account_metas.clone(),
            )],
            Some(transaction_context.get_key_of_account_at_index(0).unwrap()),
        ));
        let environment_config = EnvironmentConfig::new(
            Hash::default(),
            0,
            &MockCallback {},
            Arc::new(FeatureSet::all_enabled()),
            &sysvar_cache,
        );
        let mut invoke_context = InvokeContext::new(
            &mut transaction_context,
            &mut program_cache_for_tx_batch,
            environment_config,
            None,
            SVMTransactionExecutionBudget::default(),
            SVMTransactionExecutionCost::default(),
        );
        let result = process_message(
            &message,
            &program_indices,
            &mut invoke_context,
            &mut ExecuteTimings::default(),
            &mut 0,
        );
        assert!(result.is_ok());

        // Do work on the same transaction account but at different instruction accounts
        let message = new_sanitized_message(Message::new(
            &[Instruction::new_with_bincode(
                mock_program_id,
                &MockSystemInstruction::DoWork {
                    lamports: 10,
                    data: 42,
                },
                account_metas,
            )],
            Some(transaction_context.get_key_of_account_at_index(0).unwrap()),
        ));
        let environment_config = EnvironmentConfig::new(
            Hash::default(),
            0,
            &MockCallback {},
            Arc::new(FeatureSet::all_enabled()),
            &sysvar_cache,
        );
        let mut invoke_context = InvokeContext::new(
            &mut transaction_context,
            &mut program_cache_for_tx_batch,
            environment_config,
            None,
            SVMTransactionExecutionBudget::default(),
            SVMTransactionExecutionCost::default(),
        );
        let result = process_message(
            &message,
            &program_indices,
            &mut invoke_context,
            &mut ExecuteTimings::default(),
            &mut 0,
        );
        assert!(result.is_ok());
        assert_eq!(
            transaction_context
                .accounts()
                .try_borrow(0)
                .unwrap()
                .lamports(),
            80
        );
        assert_eq!(
            transaction_context
                .accounts()
                .try_borrow(1)
                .unwrap()
                .lamports(),
            20
        );
        assert_eq!(
            transaction_context.accounts().try_borrow(0).unwrap().data(),
            &vec![42]
        );
    }

    fn secp256k1_instruction_for_test() -> Instruction {
        let secret_key = libsecp256k1::SecretKey::random(&mut thread_rng());
        new_secp256k1_instruction(&secret_key, b"hello")
    }

    fn ed25519_instruction_for_test() -> Instruction {
        let secret_key = ed25519_dalek::Keypair::generate(&mut thread_rng());
        new_ed25519_instruction(&secret_key, b"hello")
    }

    fn secp256r1_instruction_for_test() -> Instruction {
        let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap();
        let secret_key = EcKey::generate(&group).unwrap();
        new_secp256r1_instruction(b"hello", secret_key).unwrap()
    }

    #[test]
    fn test_precompile() {
        let mock_program_id = Pubkey::new_unique();
        declare_process_instruction!(MockBuiltin, 1, |_invoke_context| {
            Err(InstructionError::Custom(0xbabb1e))
        });

        let mut secp256k1_account = AccountSharedData::new(1, 0, &native_loader::id());
        secp256k1_account.set_executable(true);
        let mut ed25519_account = AccountSharedData::new(1, 0, &native_loader::id());
        ed25519_account.set_executable(true);
        let mut secp256r1_account = AccountSharedData::new(1, 0, &native_loader::id());
        secp256r1_account.set_executable(true);
        let mut mock_program_account = AccountSharedData::new(1, 0, &native_loader::id());
        mock_program_account.set_executable(true);
        let accounts = vec![
            (
                Pubkey::new_unique(),
                AccountSharedData::new(1, 0, &system_program::id()),
            ),
            (secp256k1_program::id(), secp256k1_account),
            (ed25519_program::id(), ed25519_account),
            (solana_secp256r1_program::id(), secp256r1_account),
            (mock_program_id, mock_program_account),
        ];
        let mut transaction_context = TransactionContext::new(accounts, Rent::default(), 1, 4);

        let message = new_sanitized_message(Message::new(
            &[
                secp256k1_instruction_for_test(),
                ed25519_instruction_for_test(),
                secp256r1_instruction_for_test(),
                Instruction::new_with_bytes(mock_program_id, &[], vec![]),
            ],
            Some(transaction_context.get_key_of_account_at_index(0).unwrap()),
        ));
        let sysvar_cache = SysvarCache::default();
        let mut program_cache_for_tx_batch = ProgramCacheForTxBatch::default();
        program_cache_for_tx_batch.replenish(
            mock_program_id,
            Arc::new(ProgramCacheEntry::new_builtin(0, 0, MockBuiltin::vm)),
        );
        let environment_config = EnvironmentConfig::new(
            Hash::default(),
            0,
            &MockCallback {},
            Arc::new(FeatureSet::all_enabled()),
            &sysvar_cache,
        );
        let mut invoke_context = InvokeContext::new(
            &mut transaction_context,
            &mut program_cache_for_tx_batch,
            environment_config,
            None,
            SVMTransactionExecutionBudget::default(),
            SVMTransactionExecutionCost::default(),
        );
        let result = process_message(
            &message,
            &[vec![1], vec![2], vec![3], vec![4]],
            &mut invoke_context,
            &mut ExecuteTimings::default(),
            &mut 0,
        );

        assert_eq!(
            result,
            Err(TransactionError::InstructionError(
                3,
                InstructionError::Custom(0xbabb1e)
            ))
        );
        assert_eq!(transaction_context.get_instruction_trace_length(), 4);
    }
}
