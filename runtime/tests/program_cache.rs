use {
    agave_feature_set as feature_set,
    assert_matches::assert_matches,
    solana_account::{
        Account, AccountSharedData, WritableAccount, state_traits::StateMutWincode as _,
    },
    solana_feature_gate_interface::{self as feature, Feature},
    solana_instruction::Instruction,
    solana_instruction_error::InstructionError,
    solana_keypair::Keypair,
    solana_leader_schedule::SlotLeader,
    solana_loader_v3_interface::state::UpgradeableLoaderState,
    solana_message::Message,
    solana_native_token::LAMPORTS_PER_SOL,
    solana_program_runtime::program_cache_entry::ProgramCacheEntryType,
    solana_rent::Rent,
    solana_runtime::{
        bank::{Bank, test_utils::goto_end_of_slot},
        loader_utils::{create_buffer_with_elf, create_program_with_elf},
    },
    solana_sdk_ids::{bpf_loader, bpf_loader_upgradeable},
    solana_signer::Signer,
    solana_transaction::Transaction,
    solana_transaction_error::TransactionError,
    std::{fs::File, io::Read, sync::Arc},
};

#[test]
fn test_bank_load_program() {
    agave_logger::setup();

    let (genesis_config, mint_keypair) =
        solana_genesis_config::create_genesis_config(1_000_000_000);
    let bank = Bank::new_for_tests(&genesis_config);
    let (bank, bank_forks) = bank.wrap_with_bank_forks_for_tests();
    goto_end_of_slot(bank.clone());
    let bank = Bank::new_from_parent_with_bank_forks(&bank_forks, bank, SlotLeader::default(), 42);
    let bank = Bank::new_from_parent_with_bank_forks(&bank_forks, bank, SlotLeader::default(), 50);

    let program_key = solana_pubkey::new_rand();
    let programdata_key = solana_pubkey::new_rand();

    let mut file = File::open("../programs/bpf_loader/test_elfs/out/noop_aligned.so").unwrap();
    let mut elf = Vec::new();
    file.read_to_end(&mut elf).unwrap();
    let mut program_account = AccountSharedData::new_data(
        40,
        &UpgradeableLoaderState::Program {
            programdata_address: programdata_key,
        },
        &bpf_loader_upgradeable::id(),
    )
    .unwrap();
    program_account.set_executable(true);
    program_account.set_rent_epoch(1);
    let programdata_data_offset = UpgradeableLoaderState::size_of_programdata_metadata();
    let mut programdata_account = AccountSharedData::new(
        40,
        programdata_data_offset + elf.len(),
        &bpf_loader_upgradeable::id(),
    );
    wincode::serialize_into(
        programdata_account.data_as_mut_slice(),
        &UpgradeableLoaderState::ProgramData {
            slot: 42,
            upgrade_authority_address: None,
        },
    )
    .unwrap();
    programdata_account.data_as_mut_slice()[programdata_data_offset..].copy_from_slice(&elf);
    programdata_account.set_rent_epoch(1);
    bank.store_account(&program_key, &program_account);
    bank.store_account(&programdata_key, &programdata_account);

    let instruction = Instruction::new_with_bytes(program_key, &[], Vec::new());
    let invocation_message = Message::new(&[instruction], Some(&mint_keypair.pubkey()));
    let binding = mint_keypair.insecure_clone();
    let transaction = Transaction::new(&[&binding], invocation_message, bank.last_blockhash());
    assert!(bank.process_transaction(&transaction).is_ok());

    {
        let program_cache = bank
            .get_transaction_processor()
            .global_program_cache
            .read()
            .unwrap();
        let [program] = program_cache.get_slot_versions_for_tests(&program_key) else {
            panic!();
        };
        assert_matches!(program.program, ProgramCacheEntryType::Loaded(_));
    }
}

#[test]
fn test_feature_activation_loaded_programs_cache_preparation_phase() {
    agave_logger::setup();

    // Bank Setup
    let (mut genesis_config, mint_keypair) =
        solana_genesis_config::create_genesis_config(1_000_000 * LAMPORTS_PER_SOL);
    genesis_config
        .accounts
        .remove(&feature_set::disable_sbpf_v0_execution::id());
    genesis_config
        .accounts
        .remove(&feature_set::reenable_sbpf_v0_execution::id());
    let (root_bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

    // Program Setup
    let program_keypair = Keypair::new();
    let program_data = include_bytes!("../../programs/bpf_loader/test_elfs/out/noop_aligned.so");
    let program_account = AccountSharedData::from(Account {
        lamports: Rent::default().minimum_balance(program_data.len()).min(1),
        data: program_data.to_vec(),
        owner: bpf_loader::id(),
        executable: true,
        rent_epoch: 0,
    });
    root_bank.store_account(&program_keypair.pubkey(), &program_account);

    // Compose message using the desired program.
    let instruction = Instruction::new_with_bytes(program_keypair.pubkey(), &[], Vec::new());
    let message = Message::new(&[instruction], Some(&mint_keypair.pubkey()));
    let binding = mint_keypair.insecure_clone();
    let signers = vec![&binding];

    // Advance the bank so that the program becomes effective.
    goto_end_of_slot(root_bank.clone());
    let bank = Bank::new_from_parent_with_bank_forks(
        bank_forks.as_ref(),
        root_bank.clone(),
        SlotLeader::default(),
        root_bank.slot() + 1,
    );

    // Load the program with the old environment.
    let transaction = Transaction::new(&signers, message.clone(), bank.last_blockhash());
    let result_without_feature_enabled = bank.process_transaction(&transaction);
    assert_eq!(result_without_feature_enabled, Ok(()));

    // Schedule feature activation to trigger a change of environment at the epoch boundary.
    let feature_account_balance =
        std::cmp::max(genesis_config.rent.minimum_balance(Feature::size_of()), 1);
    bank.store_account(
        &feature_set::disable_sbpf_v0_execution::id(),
        &feature::create_account(&Feature { activated_at: None }, feature_account_balance),
    );

    // Before the recompilation phase, only the original program is cached.
    goto_end_of_slot(bank.clone());
    {
        let program_cache = bank
            .get_transaction_processor()
            .global_program_cache
            .read()
            .unwrap();
        let slot_versions = program_cache.get_slot_versions_for_tests(&program_keypair.pubkey());
        assert_eq!(slot_versions.len(), 1);
    }

    // Advance the bank to middle of epoch to start the recompilation phase,
    // which recompiles the program in the same slot it starts.
    let bank = Bank::new_from_parent_with_bank_forks(&bank_forks, bank, SlotLeader::default(), 16);
    let current_env = bank
        .get_transaction_processor()
        .program_runtime_environment_for_epoch(0);
    let upcoming_env = bank
        .get_transaction_processor()
        .program_runtime_environment_for_epoch(1);
    let ebpp_env = bank
        .get_transaction_processor()
        .epoch_boundary_preparation
        .read()
        .unwrap()
        .upcoming_environment
        .clone()
        .unwrap();
    assert!(*upcoming_env == *ebpp_env);
    assert_eq!(upcoming_env, ebpp_env); // `Arc::ptr_eq`
    {
        let program_cache = bank
            .get_transaction_processor()
            .global_program_cache
            .write()
            .unwrap();
        let slot_versions = program_cache.get_slot_versions_for_tests(&program_keypair.pubkey());
        assert_eq!(slot_versions.len(), 2);
        assert_eq!(
            slot_versions[0].program.get_environment().unwrap(),
            &current_env,
        );
        assert_eq!(
            slot_versions[1].program.get_environment().unwrap(),
            &upcoming_env,
        );
    }

    // Advance the bank to cross the epoch boundary and activate the feature.
    goto_end_of_slot(bank.clone());
    let bank = Bank::new_from_parent_with_bank_forks(&bank_forks, bank, SlotLeader::default(), 33);
    let new_processor_env = bank
        .get_transaction_processor()
        .program_runtime_environment
        .clone();
    assert!(*upcoming_env == *new_processor_env);
    assert_eq!(upcoming_env, new_processor_env);

    // The processor's new environment is equal to the EBPP-prepared one in
    // both value and `Arc::ptr_eq`. We assert below that this reflects the
    // new-epoch bank's feature set, but we can only compare by value.
    let computed_env = bank.create_program_runtime_environment(&bank.feature_set);
    assert!(*computed_env == *new_processor_env);
    assert_ne!(computed_env, new_processor_env);
    assert_ne!(computed_env, upcoming_env);

    // Load the program with the new environment.
    let transaction = Transaction::new(&signers, message, bank.last_blockhash());
    let result_with_feature_enabled = bank.process_transaction(&transaction);
    assert_eq!(
        result_with_feature_enabled,
        Err(TransactionError::InstructionError(
            0,
            InstructionError::UnsupportedProgramId
        ))
    );
}

#[test]
fn test_feature_activation_loaded_programs_epoch_transition() {
    agave_logger::setup();

    // Bank Setup
    let (mut genesis_config, mint_keypair) =
        solana_genesis_config::create_genesis_config(1_000_000 * LAMPORTS_PER_SOL);
    genesis_config
        .accounts
        .remove(&feature_set::disable_fees_sysvar::id());
    genesis_config
        .accounts
        .remove(&feature_set::reenable_sbpf_v0_execution::id());
    let (root_bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

    // Program Setup
    let program_keypair = Keypair::new();
    let program_data = include_bytes!("../../programs/bpf_loader/test_elfs/out/noop_aligned.so");
    let program_account = AccountSharedData::from(Account {
        lamports: Rent::default().minimum_balance(program_data.len()).min(1),
        data: program_data.to_vec(),
        owner: bpf_loader::id(),
        executable: true,
        rent_epoch: 0,
    });
    root_bank.store_account(&program_keypair.pubkey(), &program_account);

    // Compose message using the desired program.
    let instruction = Instruction::new_with_bytes(program_keypair.pubkey(), &[], Vec::new());
    let message = Message::new(&[instruction], Some(&mint_keypair.pubkey()));
    let binding = mint_keypair.insecure_clone();
    let signers = vec![&binding];

    // Advance the bank so that the program becomes effective.
    goto_end_of_slot(root_bank.clone());
    let bank = Bank::new_from_parent_with_bank_forks(
        bank_forks.as_ref(),
        root_bank.clone(),
        SlotLeader::default(),
        root_bank.slot() + 1,
    );

    // Load the program with the old environment.
    let transaction = Transaction::new(&signers, message.clone(), bank.last_blockhash());
    assert!(bank.process_transaction(&transaction).is_ok());

    // Schedule feature activation to trigger a change of environment at the epoch boundary.
    let feature_account_balance =
        std::cmp::max(genesis_config.rent.minimum_balance(Feature::size_of()), 1);
    bank.store_account(
        &feature_set::disable_fees_sysvar::id(),
        &feature::create_account(&Feature { activated_at: None }, feature_account_balance),
    );

    // Advance the bank to the end of the epoch to update the epoch_boundary_preparation.
    goto_end_of_slot(bank.clone());
    let bank = Bank::new_from_parent_with_bank_forks(&bank_forks, bank, SlotLeader::default(), 31);
    let current_env = bank
        .get_transaction_processor()
        .program_runtime_environment_for_epoch(0);
    let upcoming_env = bank
        .get_transaction_processor()
        .program_runtime_environment_for_epoch(1);
    let ebpp_env = bank
        .get_transaction_processor()
        .epoch_boundary_preparation
        .read()
        .unwrap()
        .upcoming_environment
        .clone()
        .unwrap();
    assert!(*current_env != *upcoming_env);
    assert!(*upcoming_env == *ebpp_env);
    assert_eq!(upcoming_env, ebpp_env); // `Arc::ptr_eq`

    // Here we see our guard kick in, for the final slot only.
    let guard_env = bank
        .get_transaction_processor()
        .deployment_env_override()
        .unwrap()
        .clone();
    assert!(*guard_env == *ebpp_env);
    assert_eq!(guard_env, ebpp_env);

    // Advance the bank to cross the epoch boundary and activate the feature.
    goto_end_of_slot(bank.clone());
    let bank = Bank::new_from_parent_with_bank_forks(&bank_forks, bank, SlotLeader::default(), 32);
    let new_processor_env = bank
        .get_transaction_processor()
        .program_runtime_environment
        .clone();
    assert!(*upcoming_env == *new_processor_env);
    assert_eq!(upcoming_env, new_processor_env);
    let computed_env = bank.create_program_runtime_environment(&bank.feature_set);
    assert!(*computed_env == *new_processor_env);
    assert_ne!(computed_env, new_processor_env);
    assert_ne!(computed_env, upcoming_env);

    // The guard is lifted now that the epoch has rolled over.
    assert!(
        bank.get_transaction_processor()
            .deployment_env_override()
            .is_none()
    );
    assert!(*bank.get_transaction_processor().program_runtime_environment == *guard_env);

    // Load the program with the new environment.
    let transaction = Transaction::new(&signers, message.clone(), bank.last_blockhash());
    assert!(bank.process_transaction(&transaction).is_ok());

    {
        // Prune for rerooting and thus finishing the recompilation phase.
        let upcoming_environment = bank
            .get_transaction_processor()
            .epoch_boundary_preparation
            .write()
            .unwrap()
            .reroot(bank.epoch());
        assert!(upcoming_environment.is_some());
        let mut program_cache = bank
            .get_transaction_processor()
            .global_program_cache
            .write()
            .unwrap();
        program_cache.prune(
            bank.slot(),
            upcoming_environment,
            &bank_forks.read().unwrap(),
        );

        // Unload all (which is only the entry with the new environment)
        program_cache.sort_and_unload(0);
    }

    // Reload the unloaded program with the new environment.
    goto_end_of_slot(bank.clone());
    let bank = Bank::new_from_parent_with_bank_forks(
        bank_forks.as_ref(),
        bank.clone(),
        SlotLeader::default(),
        bank.slot() + 1,
    );
    let transaction = Transaction::new(&signers, message, bank.last_blockhash());
    assert!(bank.process_transaction(&transaction).is_ok());
}

#[test]
fn test_feature_activation_loaded_programs_late_activation() {
    agave_logger::setup();

    // Bank Setup
    let (mut genesis_config, mint_keypair) =
        solana_genesis_config::create_genesis_config(1_000_000 * LAMPORTS_PER_SOL);
    genesis_config
        .accounts
        .remove(&feature_set::disable_sbpf_v0_execution::id());
    genesis_config
        .accounts
        .remove(&feature_set::reenable_sbpf_v0_execution::id());
    let (root_bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

    // Program Setup
    let program_keypair = Keypair::new();
    let program_data = include_bytes!("../../programs/bpf_loader/test_elfs/out/noop_aligned.so");
    let program_account = AccountSharedData::from(Account {
        lamports: Rent::default().minimum_balance(program_data.len()).min(1),
        data: program_data.to_vec(),
        owner: bpf_loader::id(),
        executable: true,
        rent_epoch: 0,
    });
    root_bank.store_account(&program_keypair.pubkey(), &program_account);

    // Compose message using the desired program.
    let instruction = Instruction::new_with_bytes(program_keypair.pubkey(), &[], Vec::new());
    let message = Message::new(&[instruction], Some(&mint_keypair.pubkey()));
    let binding = mint_keypair.insecure_clone();
    let signers = vec![&binding];

    // Advance the bank so that the program becomes effective.
    goto_end_of_slot(root_bank.clone());
    let bank = Bank::new_from_parent_with_bank_forks(
        bank_forks.as_ref(),
        root_bank.clone(),
        SlotLeader::default(),
        root_bank.slot() + 1,
    );

    // Load the program with the old environment.
    let transaction = Transaction::new(&signers, message.clone(), bank.last_blockhash());
    let result_without_feature_enabled = bank.process_transaction(&transaction);
    assert_eq!(result_without_feature_enabled, Ok(()));

    // Before the recompilation phase, only the original program is cached.
    goto_end_of_slot(bank.clone());
    {
        let program_cache = bank
            .get_transaction_processor()
            .global_program_cache
            .read()
            .unwrap();
        let slot_versions = program_cache.get_slot_versions_for_tests(&program_keypair.pubkey());
        assert_eq!(slot_versions.len(), 1);
    }

    // Advance the bank to the middle of the epoch to start the recompilation
    // phase. We've seen no feature yet, so environments should match and cache
    // contents should be unchanged. Also, EBPP should not have latched yet,
    // since it hasn't seen a changed environment yet.
    let bank = Bank::new_from_parent_with_bank_forks(&bank_forks, bank, SlotLeader::default(), 16);
    let current_env = bank
        .get_transaction_processor()
        .program_runtime_environment_for_epoch(0);
    let upcoming_env = bank
        .get_transaction_processor()
        .program_runtime_environment_for_epoch(1);
    let ebpp = bank
        .get_transaction_processor()
        .epoch_boundary_preparation
        .read()
        .unwrap();
    assert!(*current_env == *upcoming_env);
    assert_eq!(current_env, upcoming_env);
    {
        let program_cache = bank
            .get_transaction_processor()
            .global_program_cache
            .write()
            .unwrap();
        let slot_versions = program_cache.get_slot_versions_for_tests(&program_keypair.pubkey());
        assert_eq!(slot_versions.len(), 1);
        assert_eq!(
            slot_versions[0].program.get_environment().unwrap(),
            &current_env,
        );
    }
    assert!(ebpp.upcoming_environment.is_none());
    assert!(ebpp.programs_to_recompile.is_empty());
    drop(ebpp);

    // Now move forward a few more slots, mid-way into EBPP, and activate the
    // feature.
    goto_end_of_slot(bank.clone());
    let bank = Bank::new_from_parent_with_bank_forks(&bank_forks, bank, SlotLeader::default(), 24);
    let feature_account_balance =
        std::cmp::max(genesis_config.rent.minimum_balance(Feature::size_of()), 1);
    bank.store_account(
        &feature_set::disable_sbpf_v0_execution::id(),
        &feature::create_account(&Feature { activated_at: None }, feature_account_balance),
    );

    // Go a few more slots. See that the EBPP does in fact relatch, now that
    // we observe a changed upcoming environment. We also recompile our first
    // few programs.
    goto_end_of_slot(bank.clone());
    let bank = Bank::new_from_parent_with_bank_forks(&bank_forks, bank, SlotLeader::default(), 30);
    let current_env = bank
        .get_transaction_processor()
        .program_runtime_environment_for_epoch(0);
    let upcoming_env = bank
        .get_transaction_processor()
        .program_runtime_environment_for_epoch(1);
    let ebpp_env = bank
        .get_transaction_processor()
        .epoch_boundary_preparation
        .read()
        .unwrap()
        .upcoming_environment
        .clone()
        .unwrap();
    assert!(*current_env != *upcoming_env);
    assert_ne!(current_env, upcoming_env);
    assert!(*ebpp_env == *upcoming_env);
    assert_eq!(ebpp_env, upcoming_env);
    {
        let program_cache = bank
            .get_transaction_processor()
            .global_program_cache
            .write()
            .unwrap();
        let slot_versions = program_cache.get_slot_versions_for_tests(&program_keypair.pubkey());
        assert_eq!(slot_versions.len(), 2);
        assert_eq!(
            slot_versions[0].program.get_environment().unwrap(),
            &current_env,
        );
        assert_eq!(
            slot_versions[1].program.get_environment().unwrap(),
            &upcoming_env, // <-- EBPP env
        );
    }

    // Now cross the epoch boundary and activate the feature.
    goto_end_of_slot(bank.clone());
    let bank = Bank::new_from_parent_with_bank_forks(&bank_forks, bank, SlotLeader::default(), 33);

    // The processor's new environment is now exactly equal to what EBPP was
    // preparing for.
    let computed_env = bank.create_program_runtime_environment(&bank.feature_set);
    let new_processor_env = bank
        .get_transaction_processor()
        .program_runtime_environment
        .clone();
    assert!(*new_processor_env == *computed_env);
    assert!(*new_processor_env == *upcoming_env);
    assert_eq!(new_processor_env, upcoming_env); // `Arc::ptr_eq`

    // Cache contents should be unchanged. The program should have the new
    // environment.
    {
        let program_cache = bank
            .get_transaction_processor()
            .global_program_cache
            .write()
            .unwrap();
        let slot_versions = program_cache.get_slot_versions_for_tests(&program_keypair.pubkey());
        assert_eq!(slot_versions.len(), 2);
        assert_eq!(
            slot_versions[0].program.get_environment().unwrap(),
            &current_env,
        );
        assert_eq!(
            slot_versions[1].program.get_environment().unwrap(),
            &new_processor_env,
        );
    }
}

#[test]
fn test_feature_activation_loaded_programs_fork_without_activation() {
    // Fork graph created for the test
    //                              10
    //                             /  \
    //         Feature staged --> 11   16 <-- Enters window first
    //                            |    |
    //   Enters window second --> 17   18
    //
    agave_logger::setup();

    // Bank Setup
    let (mut genesis_config, mint_keypair) =
        solana_genesis_config::create_genesis_config(1_000_000 * LAMPORTS_PER_SOL);
    genesis_config
        .accounts
        .remove(&feature_set::disable_sbpf_v0_execution::id());
    genesis_config
        .accounts
        .remove(&feature_set::reenable_sbpf_v0_execution::id());
    let (root_bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

    // Program Setup (multiple)
    let program_data = include_bytes!("../../programs/bpf_loader/test_elfs/out/noop_aligned.so");
    let program_keypairs = [Keypair::new(), Keypair::new(), Keypair::new()];
    for program_keypair in &program_keypairs {
        let program_account = AccountSharedData::from(Account {
            lamports: Rent::default().minimum_balance(program_data.len()).min(1),
            data: program_data.to_vec(),
            owner: bpf_loader::id(),
            executable: true,
            rent_epoch: 0,
        });
        root_bank.store_account(&program_keypair.pubkey(), &program_account);
    }

    // Advance the bank so that the programs become effective, then load them
    // all with the old environment.
    goto_end_of_slot(root_bank.clone());
    let bank = Bank::new_from_parent_with_bank_forks(
        bank_forks.as_ref(),
        root_bank.clone(),
        SlotLeader::default(),
        root_bank.slot() + 1,
    );
    for program_keypair in &program_keypairs {
        let instruction = Instruction::new_with_bytes(program_keypair.pubkey(), &[], Vec::new());
        let message = Message::new(&[instruction], Some(&mint_keypair.pubkey()));
        let transaction = Transaction::new(&[&mint_keypair], message, bank.last_blockhash());
        assert_eq!(bank.process_transaction(&transaction), Ok(()));
    }

    // Fork point at 10.
    goto_end_of_slot(bank.clone());
    let fork_point =
        Bank::new_from_parent_with_bank_forks(&bank_forks, bank, SlotLeader::default(), 10);
    goto_end_of_slot(fork_point.clone());

    // Feature is submitted on 11, on a fork.
    let with_feature = Bank::new_from_parent_with_bank_forks(
        &bank_forks,
        fork_point.clone(),
        SlotLeader::default(),
        11,
    );
    let feature_account_balance =
        std::cmp::max(genesis_config.rent.minimum_balance(Feature::size_of()), 1);
    with_feature.store_account(
        &feature_set::disable_sbpf_v0_execution::id(),
        &feature::create_account(&Feature { activated_at: None }, feature_account_balance),
    );

    // Fork on 16 does not see the feature and enters the EBPP window first.
    // It sees no change in environment, so EBPP does not latch yet.
    let without_feature =
        Bank::new_from_parent_with_bank_forks(&bank_forks, fork_point, SlotLeader::default(), 16);
    {
        let current_env = without_feature
            .get_transaction_processor()
            .program_runtime_environment_for_epoch(0);
        let upcoming_env = without_feature
            .get_transaction_processor()
            .program_runtime_environment_for_epoch(1);
        let ebpp = without_feature
            .get_transaction_processor()
            .epoch_boundary_preparation
            .read()
            .unwrap();
        assert!(*current_env == *upcoming_env);
        assert_eq!(current_env, upcoming_env);
        assert!(ebpp.upcoming_environment.is_none());
        assert!(ebpp.programs_to_recompile.is_empty());
    }

    // The fork with the activation now enters the EBPP window at 17. It
    // latches EBPP with its observed upcoming environment.
    goto_end_of_slot(with_feature.clone());
    let with_feature =
        Bank::new_from_parent_with_bank_forks(&bank_forks, with_feature, SlotLeader::default(), 17);
    let ebpp_latch_env = {
        let current_env = without_feature
            .get_transaction_processor()
            .program_runtime_environment_for_epoch(0);
        let upcoming_env = without_feature
            .get_transaction_processor()
            .program_runtime_environment_for_epoch(1);
        let ebpp = without_feature
            .get_transaction_processor()
            .epoch_boundary_preparation
            .read()
            .unwrap();
        let ebpp_env = ebpp.upcoming_environment.clone().unwrap();
        assert!(*current_env != *upcoming_env);
        assert_ne!(current_env, upcoming_env);
        assert!(*ebpp_env == *upcoming_env);
        assert_eq!(ebpp_env, upcoming_env);
        // We already recompiled one program in this slot, too.
        assert_eq!(ebpp.programs_to_recompile.len(), program_keypairs.len() - 1);
        ebpp_env
    };

    // The fork without the activation steps through the phase again at 18. It
    // finds latch closed from slot 17's environment. However, since this fork
    // still sees no change in environment, it leaves the latch alone.
    let without_feature = Bank::new_from_parent_with_bank_forks(
        &bank_forks,
        without_feature,
        SlotLeader::default(),
        18,
    );
    let non_recompiled_programs = {
        // If we query from EBPP, we'll see the new environment from the other
        // fork.
        let current_env = without_feature
            .get_transaction_processor()
            .program_runtime_environment_for_epoch(0);
        let upcoming_env = without_feature
            .get_transaction_processor()
            .program_runtime_environment_for_epoch(1);
        assert!(*current_env != *upcoming_env);
        assert_ne!(current_env, upcoming_env);

        // But if we compare with this fork's feature set, we'll see it's not
        // the same.
        let computed_env =
            without_feature.create_program_runtime_environment(&without_feature.feature_set);
        assert!(*current_env == *computed_env);

        // EBPP is still latched properly to the "with feature" fork's upcoming
        // environment.
        let ebpp = without_feature
            .get_transaction_processor()
            .epoch_boundary_preparation
            .read()
            .unwrap();
        let ebpp_env = ebpp.upcoming_environment.clone().unwrap();
        assert!(*ebpp_env == *upcoming_env);
        assert_eq!(ebpp_env, upcoming_env);
        assert_eq!(ebpp_env, ebpp_latch_env);
        assert!(*ebpp_env != *computed_env);
        // This fork even picked up some work, even though it doesn't have
        // the feature!
        assert_eq!(ebpp.programs_to_recompile.len(), program_keypairs.len() - 2);

        // Skip these in checks later, since they didn't get recompiled.
        ebpp.programs_to_recompile.clone()
    };

    // Now the fork with the activation crosses the epoch boundary and
    // activates the feature.
    goto_end_of_slot(with_feature.clone());
    let with_feature =
        Bank::new_from_parent_with_bank_forks(&bank_forks, with_feature, SlotLeader::default(), 33);

    // The processor's new environment is now exactly equal to what EBPP was
    // preparing for.
    let computed_env = with_feature.create_program_runtime_environment(&with_feature.feature_set);
    let new_processor_env = with_feature
        .get_transaction_processor()
        .program_runtime_environment
        .clone();
    assert!(*new_processor_env == *computed_env);
    assert!(*new_processor_env == *ebpp_latch_env);
    assert_eq!(new_processor_env, ebpp_latch_env); // `Arc::ptr_eq`

    {
        let program_cache = with_feature
            .get_transaction_processor()
            .global_program_cache
            .write()
            .unwrap();
        for program_id in program_keypairs.iter().filter_map(|program_keypair| {
            let program_id = program_keypair.pubkey();
            non_recompiled_programs
                .iter()
                .all(|(key, _)| *key != program_id)
                .then_some(program_id)
        }) {
            let slot_versions = program_cache.get_slot_versions_for_tests(&program_id);
            assert_eq!(slot_versions.len(), 2);
            assert_eq!(
                slot_versions[1].program.get_environment().unwrap(),
                &new_processor_env,
            );
        }
    }
}

#[test]
fn test_sbpf_v0_deploy_in_last_slot_before_feature_activation() {
    // TODO: This test is only required while we continue to maintain two
    // program runtime environments at the end of a feature activation window.
    // Once these are consolidated, programs in the final slot of the epoch
    // will be verified against the *current* slot's environment, and we'll
    // have no need for this test anymore.
    //
    // This test asserts that in the final slot of an epoch where a program
    // runtime feature was activated, deployments are verified against the
    // *upcoming* feature set.
    let (mut genesis_config, mint_keypair) =
        solana_genesis_config::create_genesis_config(1_000_000 * LAMPORTS_PER_SOL);
    for feature_id in [
        feature_set::disable_sbpf_v0_execution::id(),
        feature_set::reenable_sbpf_v0_execution::id(),
        feature_set::disable_sbpf_v0_v1_v2_deployment::id(),
    ] {
        genesis_config.accounts.remove(&feature_id);
    }
    let (root_bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

    let elf = include_bytes!("../../programs/bpf_loader/test_elfs/out/noop_aligned.so");
    let authority_keypair = Keypair::new();
    let payer = mint_keypair.insecure_clone();

    // Stand the SBPFv0 program up before the feature is submitted.
    let program_id = create_program_with_elf(
        &root_bank,
        &bpf_loader_upgradeable::id(),
        &authority_keypair.pubkey(),
        elf,
    );

    // Advance the bank so that the program becomes effective, then load it
    // with the old environment.
    goto_end_of_slot(root_bank.clone());
    let bank = Bank::new_from_parent_with_bank_forks(
        bank_forks.as_ref(),
        root_bank.clone(),
        SlotLeader::default(),
        root_bank.slot() + 1,
    );
    let instruction = Instruction::new_with_bytes(program_id, &[], Vec::new());
    let message = Message::new(&[instruction], Some(&mint_keypair.pubkey()));
    let transaction = Transaction::new(&[&mint_keypair], message, bank.last_blockhash());
    assert_eq!(bank.process_transaction(&transaction), Ok(()));

    let upgrade_with_sbpf_v0_elf = |bank: &Arc<Bank>| {
        let buffer_address = create_buffer_with_elf(bank, &authority_keypair.pubkey(), elf);
        let message = Message::new(
            &[solana_loader_v3_interface::instruction::upgrade(
                &program_id,
                &buffer_address,
                &authority_keypair.pubkey(),
                &payer.pubkey(),
            )],
            Some(&payer.pubkey()),
        );
        bank.process_transaction(&Transaction::new(
            &[&payer, &authority_keypair],
            message,
            bank.last_blockhash(),
        ))
    };

    goto_end_of_slot(bank.clone());
    // Activate the feature before the EBPP window opens.
    let bank = Bank::new_from_parent_with_bank_forks(&bank_forks, bank, SlotLeader::default(), 12);

    // Submit `disable_sbpf_v0_execution` for activation at the next epoch boundary.
    let feature_account_balance =
        std::cmp::max(genesis_config.rent.minimum_balance(Feature::size_of()), 1);
    bank.store_account(
        &feature_set::disable_sbpf_v0_execution::id(),
        &feature::create_account(&Feature { activated_at: None }, feature_account_balance),
    );

    // Check on our environments.
    goto_end_of_slot(bank.clone());
    let bank = Bank::new_from_parent_with_bank_forks(&bank_forks, bank, SlotLeader::default(), 24);
    let current_env = bank
        .get_transaction_processor()
        .program_runtime_environment_for_epoch(0);
    let upcoming_env = bank
        .get_transaction_processor()
        .program_runtime_environment_for_epoch(1);
    let (ebpp_env, queued) = {
        let epoch_boundary_preparation = bank
            .get_transaction_processor()
            .epoch_boundary_preparation
            .read()
            .unwrap();
        (
            epoch_boundary_preparation
                .upcoming_environment
                .clone()
                .unwrap(),
            epoch_boundary_preparation.programs_to_recompile.len(),
        )
    };
    // We should see a proper EBPP preparing for the upcoming environment. The
    // queue is drained, since recompilation starts in the slot EBPP latches.
    assert!(*current_env != *upcoming_env);
    assert!(*upcoming_env == *ebpp_env);
    assert_eq!(upcoming_env, ebpp_env);
    assert_eq!(queued, 0);

    // Advance to the last slot of the epoch.
    let last_slot_in_epoch = bank.epoch_schedule().get_last_slot_in_epoch(bank.epoch());
    goto_end_of_slot(bank.clone());
    let bank = Bank::new_from_parent_with_bank_forks(
        &bank_forks,
        bank,
        SlotLeader::default(),
        last_slot_in_epoch,
    );

    // Here we see our guard kick in, for the final slot only.
    let deployment_env = bank
        .get_transaction_processor()
        .deployment_env_override()
        .unwrap()
        .clone();
    assert!(*deployment_env != *current_env);
    assert!(*deployment_env == *ebpp_env);
    assert_eq!(deployment_env, ebpp_env);

    // New deployments in the final slot are always verified against the
    // upcoming environment. Therefore, deployment of SBPFv0 should be blocked.
    assert_eq!(
        upgrade_with_sbpf_v0_elf(&bank),
        Err(TransactionError::InstructionError(
            0,
            InstructionError::InvalidAccountData
        ))
    );

    // Cross the epoch boundary; same deal.
    goto_end_of_slot(bank.clone());
    let bank = Bank::new_from_parent_with_bank_forks(
        bank_forks.as_ref(),
        bank.clone(),
        SlotLeader::default(),
        bank.slot() + 1,
    );
    assert_eq!(bank.epoch(), 1);

    // The guard is lifted now that the epoch has rolled over.
    assert!(
        bank.get_transaction_processor()
            .deployment_env_override()
            .is_none()
    );
    assert!(*bank.get_transaction_processor().program_runtime_environment == *deployment_env);

    assert_eq!(
        upgrade_with_sbpf_v0_elf(&bank),
        Err(TransactionError::InstructionError(
            0,
            InstructionError::InvalidAccountData
        ))
    );
}
