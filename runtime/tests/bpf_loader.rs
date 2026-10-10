use {
    agave_feature_set::FeatureSet,
    solana_account::{AccountSharedData, ReadableAccount, WritableAccount},
    solana_client_traits::SyncClient,
    solana_instruction::{AccountMeta, Instruction},
    solana_instruction_error::InstructionError,
    solana_keypair::Keypair,
    solana_leader_schedule::SlotLeader,
    solana_loader_v3_interface::{
        get_program_data_address, instruction::UpgradeableLoaderInstruction,
        state::UpgradeableLoaderState,
    },
    solana_message::Message,
    solana_native_token::LAMPORTS_PER_SOL,
    solana_program_runtime::program_cache_entry::ProgramCacheEntryType,
    solana_pubkey::Pubkey,
    solana_runtime::{
        bank::{Bank, test_utils::goto_end_of_slot},
        bank_client::BankClient,
    },
    solana_sdk_ids::bpf_loader_upgradeable,
    solana_signer::Signer,
    solana_system_interface::{instruction as system_instruction, program as system_program},
    solana_sysvar as sysvar,
    solana_transaction::Transaction,
    solana_transaction_error::TransactionError,
    std::{fs::File, io::Read, sync::Arc},
};

#[test]
fn test_bpf_loader_upgradeable_deploy_with_max_len() {
    let (genesis_config, mint_keypair) =
        solana_genesis_config::create_genesis_config(1_000_000_000);
    let mut bank = Bank::new_for_tests(&genesis_config);
    let mut feature_set = FeatureSet::all_enabled();
    feature_set.deactivate(&agave_feature_set::disable_sbpf_v0_v1_v2_deployment::id());
    bank.feature_set = Arc::new(feature_set);
    let (bank, bank_forks) = bank.wrap_with_bank_forks_for_tests();
    let mut bank_client = BankClient::new_shared(bank.clone());

    // Setup keypairs and addresses
    let payer_keypair = Keypair::new();
    let program_keypair = Keypair::new();
    let buffer_address = Pubkey::new_unique();
    let (programdata_address, _) = Pubkey::find_program_address(
        &[program_keypair.pubkey().as_ref()],
        &bpf_loader_upgradeable::id(),
    );
    let upgrade_authority_keypair = Keypair::new();

    // Test nonexistent program invocation
    let instruction = Instruction::new_with_bytes(program_keypair.pubkey(), &[], Vec::new());
    let invocation_message = Message::new(&[instruction], Some(&mint_keypair.pubkey()));
    let binding = mint_keypair.insecure_clone();
    let transaction = Transaction::new(
        &[&binding],
        invocation_message.clone(),
        bank.last_blockhash(),
    );
    assert_eq!(
        bank.process_transaction(&transaction),
        Err(TransactionError::ProgramAccountNotFound),
    );
    {
        // Make sure it is not in the cache because the account owner is not a loader
        let program_cache = bank
            .get_transaction_processor()
            .global_program_cache
            .read()
            .unwrap();
        let slot_versions = program_cache.get_slot_versions_for_tests(&program_keypair.pubkey());
        assert!(slot_versions.is_empty());
    }

    // Advance bank to get a new last blockhash so that when we retry invocation
    // after creating the program, the new transaction created below with the
    // same `invocation_message` as above doesn't return `AlreadyProcessed` when
    // processed.
    goto_end_of_slot(bank);
    let bank = bank_client
        .advance_slot(1, bank_forks.as_ref(), SlotLeader::default())
        .unwrap();

    // Load program file
    let mut file = File::open("../programs/bpf_loader/test_elfs/out/noop_aligned.so")
        .expect("file open failed");
    let mut elf = Vec::new();
    file.read_to_end(&mut elf).unwrap();

    // Compute rent exempt balances
    let program_len = elf.len();
    let min_program_balance =
        bank.get_minimum_balance_for_rent_exemption(UpgradeableLoaderState::size_of_program());
    let min_buffer_balance = bank.get_minimum_balance_for_rent_exemption(
        UpgradeableLoaderState::size_of_buffer(program_len),
    );
    let min_programdata_balance = bank.get_minimum_balance_for_rent_exemption(
        UpgradeableLoaderState::size_of_programdata(program_len),
    );

    // Setup accounts
    let buffer_account = {
        let mut account = AccountSharedData::new(
            min_buffer_balance,
            UpgradeableLoaderState::size_of_buffer(elf.len()),
            &bpf_loader_upgradeable::id(),
        );
        wincode::serialize_into(
            account.data_as_mut_slice(),
            &UpgradeableLoaderState::Buffer {
                authority_address: Some(upgrade_authority_keypair.pubkey()),
            },
        )
        .unwrap();
        account
            .data_as_mut_slice()
            .get_mut(UpgradeableLoaderState::size_of_buffer_metadata()..)
            .unwrap()
            .copy_from_slice(&elf);
        account
    };
    let program_account = AccountSharedData::new(
        min_program_balance,
        UpgradeableLoaderState::size_of_program(),
        &bpf_loader_upgradeable::id(),
    );

    // Test uninitialized program invocation
    bank.store_account(&program_keypair.pubkey(), &program_account);
    let transaction = Transaction::new(
        &[&binding],
        invocation_message.clone(),
        bank.last_blockhash(),
    );
    assert_eq!(
        bank.process_transaction(&transaction),
        Err(TransactionError::InstructionError(
            0,
            InstructionError::UnsupportedProgramId
        )),
    );
    {
        let program_cache = bank
            .get_transaction_processor()
            .global_program_cache
            .read()
            .unwrap();
        let slot_versions = program_cache.get_slot_versions_for_tests(&program_keypair.pubkey());
        assert!(slot_versions.is_empty());
    }

    // Test buffer invocation
    bank.store_account(&buffer_address, &buffer_account);
    let instruction = Instruction::new_with_bytes(buffer_address, &[], Vec::new());
    let message = Message::new(&[instruction], Some(&mint_keypair.pubkey()));
    let transaction = Transaction::new(&[&binding], message, bank.last_blockhash());
    assert_eq!(
        bank.process_transaction(&transaction),
        Err(TransactionError::InstructionError(
            0,
            InstructionError::UnsupportedProgramId,
        )),
    );
    {
        let program_cache = bank
            .get_transaction_processor()
            .global_program_cache
            .read()
            .unwrap();
        let slot_versions = program_cache.get_slot_versions_for_tests(&buffer_address);
        assert!(slot_versions.is_empty());
    }

    // Test successful deploy
    let payer_base_balance = LAMPORTS_PER_SOL;
    let deploy_fees = {
        let fee_calculator = genesis_config.fee_rate_governor.create_fee_calculator();
        3 * fee_calculator.lamports_per_signature
    };
    let min_payer_balance = min_program_balance
        .saturating_add(min_programdata_balance)
        .saturating_sub(min_buffer_balance.saturating_add(deploy_fees));
    bank.store_account(
        &payer_keypair.pubkey(),
        &AccountSharedData::new(
            payer_base_balance.saturating_add(min_payer_balance),
            0,
            &system_program::id(),
        ),
    );
    bank.store_account(&program_keypair.pubkey(), &AccountSharedData::default());
    bank.store_account(&programdata_address, &AccountSharedData::default());
    let message = Message::new(
        &solana_loader_v3_interface::instruction::deploy_with_max_program_len(
            &payer_keypair.pubkey(),
            &program_keypair.pubkey(),
            &buffer_address,
            &upgrade_authority_keypair.pubkey(),
            min_program_balance,
            elf.len(),
        )
        .unwrap(),
        Some(&payer_keypair.pubkey()),
    );
    assert!(
        bank_client
            .send_and_confirm_message(
                &[&payer_keypair, &program_keypair, &upgrade_authority_keypair],
                message
            )
            .is_ok()
    );
    assert_eq!(
        bank.get_balance(&payer_keypair.pubkey()),
        payer_base_balance
    );
    assert_eq!(bank.get_balance(&buffer_address), 0);
    assert_eq!(None, bank.get_account(&buffer_address));
    let post_program_account = bank.get_account(&program_keypair.pubkey()).unwrap();
    assert_eq!(post_program_account.lamports(), min_program_balance);
    assert_eq!(post_program_account.owner(), &bpf_loader_upgradeable::id());
    assert_eq!(
        post_program_account.data().len(),
        UpgradeableLoaderState::size_of_program()
    );
    let state: UpgradeableLoaderState = wincode::deserialize(post_program_account.data()).unwrap();
    assert_eq!(
        state,
        UpgradeableLoaderState::Program {
            programdata_address
        }
    );
    let post_programdata_account = bank.get_account(&programdata_address).unwrap();
    assert_eq!(post_programdata_account.lamports(), min_programdata_balance);
    assert_eq!(
        post_programdata_account.owner(),
        &bpf_loader_upgradeable::id()
    );
    let state: UpgradeableLoaderState =
        wincode::deserialize(post_programdata_account.data()).unwrap();
    assert_eq!(
        state,
        UpgradeableLoaderState::ProgramData {
            slot: bank_client.get_slot().unwrap(),
            upgrade_authority_address: Some(upgrade_authority_keypair.pubkey())
        }
    );
    for (i, byte) in post_programdata_account
        .data()
        .get(UpgradeableLoaderState::size_of_programdata_metadata()..)
        .unwrap()
        .iter()
        .enumerate()
    {
        assert_eq!(*elf.get(i).unwrap(), *byte);
    }

    // Advance the bank so that the program becomes effective
    goto_end_of_slot(bank);
    let bank = bank_client
        .advance_slot(1, bank_forks.as_ref(), SlotLeader::default())
        .unwrap();

    // Invoke the deployed program
    let transaction = Transaction::new(&[&binding], invocation_message, bank.last_blockhash());
    assert!(bank.process_transaction(&transaction).is_ok());
    {
        let program_cache = bank
            .get_transaction_processor()
            .global_program_cache
            .read()
            .unwrap();
        let slot_versions = program_cache.get_slot_versions_for_tests(&program_keypair.pubkey());
        assert_eq!(slot_versions.len(), 1);
        assert_eq!(slot_versions[0].deployment_slot, bank.slot() - 1);
        assert_eq!(slot_versions[0].effective_slot(), bank.slot());
        assert!(matches!(
            slot_versions[0].program,
            ProgramCacheEntryType::Loaded(_),
        ));
    }
}

#[test]
fn test_bpf_loader_upgradeable_deploy_with_more_than_255_accounts() {
    let (genesis_config, _mint_keypair) =
        solana_genesis_config::create_genesis_config(1_000_000_000);
    let bank = Bank::new_for_tests(&genesis_config);
    let (bank, _bank_forks) = bank.wrap_with_bank_forks_for_tests();
    let bank_client = BankClient::new_shared(bank.clone());

    // Setup keypairs and addresses
    let payer_keypair = Keypair::new();
    let program_keypair = Keypair::new();
    let buffer_address = Pubkey::new_unique();
    let (programdata_address, _) = Pubkey::find_program_address(
        &[program_keypair.pubkey().as_ref()],
        &bpf_loader_upgradeable::id(),
    );
    let upgrade_authority_keypair = Keypair::new();

    // Load program file
    let mut file = File::open("../programs/bpf_loader/test_elfs/out/noop_aligned.so")
        .expect("file open failed");
    let mut elf = Vec::new();
    file.read_to_end(&mut elf).unwrap();

    // Compute rent exempt balances
    let program_len = elf.len();
    let min_program_balance =
        bank.get_minimum_balance_for_rent_exemption(UpgradeableLoaderState::size_of_program());
    let min_buffer_balance = bank.get_minimum_balance_for_rent_exemption(
        UpgradeableLoaderState::size_of_buffer(program_len),
    );
    let min_programdata_balance = bank.get_minimum_balance_for_rent_exemption(
        UpgradeableLoaderState::size_of_programdata(program_len),
    );

    // Setup accounts
    let buffer_account = {
        let mut account = AccountSharedData::new(
            min_buffer_balance,
            UpgradeableLoaderState::size_of_buffer(elf.len()),
            &bpf_loader_upgradeable::id(),
        );
        wincode::serialize_into(
            account.data_as_mut_slice(),
            &UpgradeableLoaderState::Buffer {
                authority_address: Some(upgrade_authority_keypair.pubkey()),
            },
        )
        .unwrap();
        account
            .data_as_mut_slice()
            .get_mut(UpgradeableLoaderState::size_of_buffer_metadata()..)
            .unwrap()
            .copy_from_slice(&elf);
        account
    };

    // Test successful deploy
    let payer_base_balance = LAMPORTS_PER_SOL;
    let deploy_fees = {
        let fee_calculator = genesis_config.fee_rate_governor.create_fee_calculator();
        3 * fee_calculator.lamports_per_signature
    };
    let min_payer_balance = min_program_balance
        .saturating_add(min_programdata_balance)
        .saturating_sub(min_buffer_balance.saturating_add(deploy_fees));
    bank.store_account(
        &payer_keypair.pubkey(),
        &AccountSharedData::new(
            payer_base_balance.saturating_add(min_payer_balance),
            0,
            &system_program::id(),
        ),
    );
    bank.store_account(&program_keypair.pubkey(), &AccountSharedData::default());
    bank.store_account(&programdata_address, &AccountSharedData::default());
    bank.store_account(&buffer_address, &buffer_account);

    fn deploy_with_max_program_len(
        payer_address: &Pubkey,
        program_address: &Pubkey,
        buffer_address: &Pubkey,
        upgrade_authority_address: &Pubkey,
        program_lamports: u64,
        max_data_len: usize,
    ) -> std::result::Result<Vec<Instruction>, InstructionError> {
        let programdata_address = get_program_data_address(program_address);
        let dummy_pubkey = Pubkey::new_unique();
        let mut deploy_ix_accounts = vec![
            AccountMeta::new(*payer_address, true),
            AccountMeta::new(programdata_address, false),
            AccountMeta::new(*program_address, false),
            AccountMeta::new(*buffer_address, false),
            AccountMeta::new_readonly(sysvar::rent::id(), false),
            AccountMeta::new_readonly(sysvar::clock::id(), false),
            AccountMeta::new_readonly(dummy_pubkey, false),
            AccountMeta::new_readonly(*upgrade_authority_address, true),
        ];
        while deploy_ix_accounts.len() < 256 {
            deploy_ix_accounts.push(AccountMeta::new_readonly(dummy_pubkey, false));
        }

        Ok(vec![
            system_instruction::create_account(
                payer_address,
                program_address,
                program_lamports,
                UpgradeableLoaderState::size_of_program() as u64,
                &bpf_loader_upgradeable::id(),
            ),
            Instruction::new_with_wincode(
                bpf_loader_upgradeable::id(),
                &UpgradeableLoaderInstruction::DeployWithMaxDataLen { max_data_len },
                deploy_ix_accounts,
            ),
        ])
    }

    let message = Message::new(
        &deploy_with_max_program_len(
            &payer_keypair.pubkey(),
            &program_keypair.pubkey(),
            &buffer_address,
            &upgrade_authority_keypair.pubkey(),
            min_program_balance,
            elf.len(),
        )
        .unwrap(),
        Some(&payer_keypair.pubkey()),
    );
    assert!(
        bank_client
            .send_and_confirm_message(
                &[&payer_keypair, &program_keypair, &upgrade_authority_keypair],
                message
            )
            .is_err()
    );
}
