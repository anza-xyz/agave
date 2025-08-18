#[cfg(test)]
mod tests_upgrade_loader_v2_program_with_loader_v3_program {
    use {
        crate::bank::{
            builtins::core_bpf_migration::tests::TestContext,
            test_utils::goto_end_of_slot,
            tests::{create_genesis_config, new_bank_from_parent_with_bank_forks},
            Bank,
        },
        agave_feature_set::FeatureSet,
        solana_account::{AccountSharedData, ReadableAccount, WritableAccount},
        solana_epoch_schedule::EpochSchedule,
        solana_feature_gate_interface::{self as feature, Feature},
        solana_instruction::{AccountMeta, Instruction},
        solana_message::Message,
        solana_native_token::LAMPORTS_PER_SOL,
        solana_program_runtime::loaded_programs::ProgramCacheEntry,
        solana_pubkey::Pubkey,
        solana_sdk_ids::bpf_loader,
        solana_signer::Signer,
        solana_transaction::Transaction,
        std::sync::Arc,
        test_case::test_case,
    };

    // CPI mockup to test CPI to newly migrated programs.
    mod cpi_mockup {
        use {
            solana_instruction::Instruction, solana_program_runtime::declare_process_instruction,
        };

        declare_process_instruction!(Entrypoint, 0, |invoke_context| {
            let transaction_context = &invoke_context.transaction_context;
            let instruction_context = transaction_context.get_current_instruction_context()?;

            let target_program_id = transaction_context.get_key_of_account_at_index(
                instruction_context.get_index_of_instruction_account_in_transaction(0)?,
            )?;

            let instruction = Instruction::new_with_bytes(*target_program_id, &[], Vec::new());

            invoke_context.native_invoke(instruction, &[])
        });
    }

    /// Mock BPF loader v2 program for testing.
    fn mock_bpf_loader_v2_program(bank: &Bank) -> AccountSharedData {
        let elf = [4u8; 200]; // Mock ELF to start.
        let space = elf.len();
        let lamports = bank.get_minimum_balance_for_rent_exemption(space);
        let owner = &bpf_loader::id();

        let mut account = AccountSharedData::new(lamports, space, owner);
        account.set_executable(true);
        account.data_as_mut_slice().copy_from_slice(&elf);

        account
    }

    #[test_case(
        agave_feature_set::replace_spl_token_with_p_token::id(),
        agave_feature_set::replace_spl_token_with_p_token::SPL_TOKEN_PROGRAM_ID,
        agave_feature_set::replace_spl_token_with_p_token::PTOKEN_PROGRAM_BUFFER;
        "p-token"
    )]
    fn test_upgrade_loader_v2_program_with_loader_v3_program(
        feature_id: Pubkey,
        program_id: Pubkey,
        source_buffer_address: Pubkey,
    ) {
        let (mut genesis_config, mint_keypair) =
            create_genesis_config(1_000_000 * LAMPORTS_PER_SOL);
        let slots_per_epoch = 32;
        genesis_config.epoch_schedule =
            EpochSchedule::custom(slots_per_epoch, slots_per_epoch, false);

        let mut root_bank = Bank::new_for_tests(&genesis_config);

        // Set up a mock BPF loader v2 program.
        {
            let program_account = mock_bpf_loader_v2_program(&root_bank);
            root_bank.store_account_and_update_capitalization(&program_id, &program_account);
            assert_eq!(
                &root_bank.get_account(&program_id).unwrap(),
                &program_account
            );
        };

        // Set up the CPI mockup to test CPI'ing to the migrated program.
        let cpi_program_id = Pubkey::new_unique();
        let cpi_program_name = "mock_cpi_program";
        root_bank.transaction_processor.add_builtin(
            &root_bank,
            cpi_program_id,
            cpi_program_name,
            ProgramCacheEntry::new_builtin(0, cpi_program_name.len(), cpi_mockup::Entrypoint::vm),
        );

        // Add the feature to the bank's inactive feature set.
        // Note this will add the feature ID if it doesn't exist.
        let mut feature_set = FeatureSet::all_enabled();
        feature_set.deactivate(&feature_id);
        root_bank.feature_set = Arc::new(feature_set);

        // Initialize the source buffer account.
        let test_context = TestContext::new(&root_bank, &program_id, &source_buffer_address, None);

        let (bank, bank_forks) = root_bank.wrap_with_bank_forks_for_tests();

        // Advance to the next epoch without activating the feature.
        let mut first_slot_in_next_epoch = slots_per_epoch + 1;
        let bank = new_bank_from_parent_with_bank_forks(
            &bank_forks,
            bank,
            &Pubkey::default(),
            first_slot_in_next_epoch,
        );

        // Assert the feature was not activated and the program was not
        // migrated.
        assert!(!bank.feature_set.is_active(&feature_id));
        assert!(bank.get_account(&source_buffer_address).is_some());

        // Store the account to activate the feature.
        bank.store_account_and_update_capitalization(
            &feature_id,
            &feature::create_account(&Feature::default(), 42),
        );

        // Advance the bank to cross the epoch boundary and activate the
        // feature.
        goto_end_of_slot(bank.clone());
        first_slot_in_next_epoch += slots_per_epoch;
        let migration_slot = first_slot_in_next_epoch;
        let bank = new_bank_from_parent_with_bank_forks(
            &bank_forks,
            bank,
            &Pubkey::default(),
            first_slot_in_next_epoch,
        );

        // Run the post-migration program checks.
        assert!(bank.feature_set.is_active(&feature_id));
        test_context.run_program_checks(&bank, migration_slot);

        // Advance one slot so that the new BPF loader v3 program becomes
        // effective in the program cache.
        goto_end_of_slot(bank.clone());
        let next_slot = bank.slot() + 1;
        let bank =
            new_bank_from_parent_with_bank_forks(&bank_forks, bank, &Pubkey::default(), next_slot);

        // Successfully invoke the new BPF loader v3 program.
        bank.process_transaction(&Transaction::new(
            &vec![&mint_keypair],
            Message::new(
                &[Instruction::new_with_bytes(program_id, &[], Vec::new())],
                Some(&mint_keypair.pubkey()),
            ),
            bank.last_blockhash(),
        ))
        .unwrap();

        // Successfully invoke the new BPF loader v3 program via CPI.
        bank.process_transaction(&Transaction::new(
            &vec![&mint_keypair],
            Message::new(
                &[Instruction::new_with_bytes(
                    cpi_program_id,
                    &[],
                    vec![AccountMeta::new_readonly(program_id, false)],
                )],
                Some(&mint_keypair.pubkey()),
            ),
            bank.last_blockhash(),
        ))
        .unwrap();

        // Simulate crossing another epoch boundary for a new bank.
        goto_end_of_slot(bank.clone());
        first_slot_in_next_epoch += slots_per_epoch;
        let bank = new_bank_from_parent_with_bank_forks(
            &bank_forks,
            bank,
            &Pubkey::default(),
            first_slot_in_next_epoch,
        );

        // Run the post-migration program checks again.
        assert!(bank.feature_set.is_active(&feature_id));
        test_context.run_program_checks(&bank, migration_slot);

        // Again, successfully invoke the new BPF loader v3 program.
        bank.process_transaction(&Transaction::new(
            &vec![&mint_keypair],
            Message::new(
                &[Instruction::new_with_bytes(program_id, &[], Vec::new())],
                Some(&mint_keypair.pubkey()),
            ),
            bank.last_blockhash(),
        ))
        .unwrap();

        // Again, successfully invoke the new BPF loader v3 program via CPI.
        bank.process_transaction(&Transaction::new(
            &vec![&mint_keypair],
            Message::new(
                &[Instruction::new_with_bytes(
                    cpi_program_id,
                    &[],
                    vec![AccountMeta::new_readonly(program_id, false)],
                )],
                Some(&mint_keypair.pubkey()),
            ),
            bank.last_blockhash(),
        ))
        .unwrap();
    }

    // Simulate a failure to migrate the program.
    // Here we want to see that the bank handles the failure gracefully and
    // advances to the next epoch without issue.
    #[test]
    fn test_core_bpf_migration_failure() {
        let (genesis_config, _mint_keypair) = create_genesis_config(0);
        let mut root_bank = Bank::new_for_tests(&genesis_config);

        let feature_id = &agave_feature_set::replace_spl_token_with_p_token::id();
        let program_id = &agave_feature_set::replace_spl_token_with_p_token::SPL_TOKEN_PROGRAM_ID;
        let source_buffer_address =
            &agave_feature_set::replace_spl_token_with_p_token::PTOKEN_PROGRAM_BUFFER;

        // Set up a mock BPF loader v2 program.
        {
            let program_account = mock_bpf_loader_v2_program(&root_bank);
            root_bank.store_account_and_update_capitalization(program_id, &program_account);
            assert_eq!(
                &root_bank.get_account(program_id).unwrap(),
                &program_account
            );
        };

        // Add the feature to the bank's inactive feature set.
        let mut feature_set = FeatureSet::all_enabled();
        feature_set.inactive_mut().insert(*feature_id);
        root_bank.feature_set = Arc::new(feature_set);

        // Initialize the source buffer account.
        let _test_context = TestContext::new(&root_bank, program_id, source_buffer_address, None);

        let (bank, bank_forks) = root_bank.wrap_with_bank_forks_for_tests();

        // Intentionally nuke the source buffer account to force the migration
        // to fail.
        bank.store_account_and_update_capitalization(
            source_buffer_address,
            &AccountSharedData::default(),
        );

        // Activate the feature.
        bank.store_account_and_update_capitalization(
            feature_id,
            &feature::create_account(&Feature::default(), 42),
        );

        // Advance the bank to cross the epoch boundary and activate the
        // feature.
        goto_end_of_slot(bank.clone());
        let bank = new_bank_from_parent_with_bank_forks(&bank_forks, bank, &Pubkey::default(), 33);

        // Assert the feature _was_ activated but the program was not migrated.
        assert!(bank.feature_set.is_active(feature_id));
        assert_eq!(
            bank.get_account(program_id).unwrap().owner(),
            &bpf_loader::id()
        );

        // Simulate crossing an epoch boundary again.
        goto_end_of_slot(bank.clone());
        let bank = new_bank_from_parent_with_bank_forks(&bank_forks, bank, &Pubkey::default(), 96);

        // Again, assert the feature is still active and the program still was
        // not migrated.
        assert!(bank.feature_set.is_active(feature_id));
        assert_eq!(
            bank.get_account(program_id).unwrap().owner(),
            &bpf_loader::id()
        );
    }
}
