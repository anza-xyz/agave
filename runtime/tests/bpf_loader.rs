#![allow(clippy::arithmetic_side_effects)]

use {
    agave_feature_set::{FeatureSet, loader_v3_set_program_data_to_elf_length},
    solana_account::{
        AccountSharedData, ReadableAccount, WritableAccount, state_traits::StateMutWincode as _,
    },
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
    solana_rent::Rent,
    solana_runtime::{
        bank::{Bank, test_utils::goto_end_of_slot},
        bank_client::BankClient,
        bank_forks::BankForks,
        genesis_utils::{GenesisConfigInfo, create_genesis_config},
        loader_utils::{create_buffer_with_elf, create_program_with_elf},
        program_cache_utils::{EntryType, ExpectedEntry, assert_cache_contents, current_env},
    },
    solana_sdk_ids::bpf_loader_upgradeable,
    solana_signer::Signer,
    solana_system_interface::{instruction as system_instruction, program as system_program},
    solana_sysvar as sysvar,
    solana_transaction::Transaction,
    solana_transaction_error::{TransactionError, TransactionResult},
    std::sync::{Arc, RwLock},
    test_case::test_case,
};

fn read_test_elf(name: &str) -> Vec<u8> {
    std::fs::read(format!("../programs/bpf_loader/test_elfs/out/{name}.so")).unwrap()
}

fn next_bank(bank_forks: &RwLock<BankForks>, bank: Arc<Bank>) -> Arc<Bank> {
    let slot = bank.slot() + 1;
    goto_end_of_slot(bank.clone());
    Bank::new_from_parent_with_bank_forks(bank_forks, bank, SlotLeader::default(), slot)
}

fn invoke(bank: &Bank, payer: &Keypair, program_id: &Pubkey) -> TransactionResult<()> {
    let instruction = Instruction::new_with_bytes(*program_id, &[], vec![]);
    let message = Message::new(&[instruction], Some(&payer.pubkey()));
    bank.process_transaction(&Transaction::new(&[payer], message, bank.last_blockhash()))
}

fn upgrade(
    bank: &Bank,
    authority: &Keypair,
    program_id: &Pubkey,
    buffer_address: &Pubkey,
    spill_address: &Pubkey,
) -> TransactionResult<()> {
    let instruction = solana_loader_v3_interface::instruction::upgrade(
        program_id,
        buffer_address,
        &authority.pubkey(),
        spill_address,
    );
    let message = Message::new(&[instruction], Some(&authority.pubkey()));
    bank.process_transaction(&Transaction::new(
        &[authority],
        message,
        bank.last_blockhash(),
    ))
}

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
    assert_eq!(
        invoke(&bank, &mint_keypair, &program_keypair.pubkey()),
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
    let elf = read_test_elf("noop_aligned");

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
        account
            .set_state(&UpgradeableLoaderState::Buffer {
                authority_address: Some(upgrade_authority_keypair.pubkey()),
            })
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
    assert_eq!(
        invoke(&bank, &mint_keypair, &program_keypair.pubkey()),
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
    assert_eq!(
        invoke(&bank, &mint_keypair, &buffer_address),
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
    let state: UpgradeableLoaderState = post_program_account.state().unwrap();
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
    let state: UpgradeableLoaderState = post_programdata_account.state().unwrap();
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
    assert_eq!(
        invoke(&bank, &mint_keypair, &program_keypair.pubkey()),
        Ok(())
    );
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
    let elf = read_test_elf("noop_aligned");

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
        account
            .set_state(&UpgradeableLoaderState::Buffer {
                authority_address: Some(upgrade_authority_keypair.pubkey()),
            })
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

fn upgrade_and_assert_resized(bank: &Bank, authority: &Keypair, program_id: &Pubkey, elf: &[u8]) {
    let programdata_address = get_program_data_address(program_id);
    let old_len = bank.get_account(&programdata_address).unwrap().data().len();
    let new_len = UpgradeableLoaderState::size_of_programdata(elf.len());
    let buffer_address = create_buffer_with_elf(bank, &authority.pubkey(), elf);
    let spill_address = Pubkey::new_unique();

    let programdata_lamports = bank.get_balance(&programdata_address);
    let buffer_lamports = bank.get_balance(&buffer_address);
    let payer_lamports = bank.get_balance(&authority.pubkey());
    let capitalization = bank.calculate_capitalization_for_tests();
    let data_size_delta = bank.load_accounts_data_size_delta_on_chain();

    assert_eq!(
        upgrade(bank, authority, program_id, &buffer_address, &spill_address),
        Ok(()),
    );

    // Programdata holds exactly the new ELF, funded to rent exemption.
    let programdata = bank.get_account(&programdata_address).unwrap();
    let rent_exempt_lamports = bank.get_minimum_balance_for_rent_exemption(new_len);
    assert_eq!(programdata.data().len(), new_len);
    assert_eq!(
        &programdata.data()[UpgradeableLoaderState::size_of_programdata_metadata()..],
        elf,
    );
    assert_eq!(programdata.lamports(), rent_exempt_lamports);

    // The buffer is drained and the surplus spilled.
    assert_eq!(bank.get_account(&buffer_address), None);
    assert_eq!(
        bank.get_balance(&spill_address),
        programdata_lamports + buffer_lamports - rent_exempt_lamports,
    );

    // Only the fee leaves circulation.
    let fee = payer_lamports - bank.get_balance(&authority.pubkey());
    assert_eq!(
        bank.calculate_capitalization_for_tests(),
        capitalization - fee,
    );

    // Data size tracks the programdata resize and the closed buffer.
    let buffer_len = UpgradeableLoaderState::size_of_buffer(elf.len());
    assert_eq!(
        bank.load_accounts_data_size_delta_on_chain() - data_size_delta,
        new_len as i64 - old_len as i64 - buffer_len as i64,
    );
}

#[test_case(false; "grow")]
#[test_case(true; "shrink")]
fn test_simd_0433_upgrade_resizes_programdata(shrink: bool) {
    let old_elf = read_test_elf("sbpfv3_return_err");
    let new_elf = read_test_elf("sbpfv3_return_ok");

    let mut programdata_elf = old_elf.clone();
    if shrink {
        // Fatten the programdata so we see a shrink.
        programdata_elf.resize(old_elf.len() * 2, 0);
    }
    assert_eq!(shrink, programdata_elf.len() > new_elf.len());

    let GenesisConfigInfo {
        mut genesis_config,
        mint_keypair: authority,
        ..
    } = create_genesis_config(LAMPORTS_PER_SOL);
    genesis_config.rent = Rent::default();
    let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    assert!(
        bank.feature_set
            .is_active(&loader_v3_set_program_data_to_elf_length::id())
    );
    let program_id = create_program_with_elf(
        &bank,
        &bpf_loader_upgradeable::id(),
        &authority.pubkey(),
        &programdata_elf,
    );
    let bank = next_bank(&bank_forks, bank);
    upgrade_and_assert_resized(&bank, &authority, &program_id, &new_elf);

    // The upgrade loaded the old program, not the new one.
    let env = current_env(&bank);
    assert_cache_contents(
        &bank,
        &program_id,
        &[ExpectedEntry {
            entry_type: EntryType::Loaded,
            deployment_slot: 0,
            environment: &env,
        }],
    );

    // The new program isn't visible until the next slot.
    assert_eq!(
        invoke(&bank, &authority, &program_id),
        Err(TransactionError::InstructionError(
            0,
            InstructionError::UnsupportedProgramId
        ))
    );

    // The resized programdata loads in the next slot.
    let upgrade_slot = bank.slot();
    let bank = next_bank(&bank_forks, bank);
    assert_eq!(invoke(&bank, &authority, &program_id), Ok(()));
    assert_cache_contents(
        &bank,
        &program_id,
        &[0, upgrade_slot].map(|deployment_slot| ExpectedEntry {
            entry_type: EntryType::Loaded,
            deployment_slot,
            environment: &env,
        }),
    );
}

#[test]
fn test_simd_0433_underfunded_grow_rolls_back() {
    let old_elf = read_test_elf("sbpfv3_return_err");
    let new_elf = read_test_elf("sbpfv3_return_ok");
    let GenesisConfigInfo {
        mut genesis_config,
        mint_keypair: authority,
        ..
    } = create_genesis_config(LAMPORTS_PER_SOL);
    genesis_config.rent = Rent::default();
    let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    assert!(
        bank.feature_set
            .is_active(&loader_v3_set_program_data_to_elf_length::id())
    );
    let program_id = create_program_with_elf(
        &bank,
        &bpf_loader_upgradeable::id(),
        &authority.pubkey(),
        &old_elf,
    );
    let bank = next_bank(&bank_forks, bank);
    let programdata_address = get_program_data_address(&program_id);

    // Leave the buffer one lamport short of funding the larger programdata.
    let buffer_address = create_buffer_with_elf(&bank, &authority.pubkey(), &new_elf);
    let required_lamports = bank.get_minimum_balance_for_rent_exemption(
        UpgradeableLoaderState::size_of_programdata(new_elf.len()),
    );
    let mut buffer = bank.get_account(&buffer_address).unwrap();
    buffer.set_lamports(required_lamports - bank.get_balance(&programdata_address) - 1);
    bank.store_account(&buffer_address, &buffer);

    let programdata = bank.get_account(&programdata_address).unwrap();
    let payer_lamports = bank.get_balance(&authority.pubkey());
    let capitalization = bank.calculate_capitalization_for_tests();
    let data_size_delta = bank.load_accounts_data_size_delta_on_chain();

    assert_eq!(
        upgrade(
            &bank,
            &authority,
            &program_id,
            &buffer_address,
            &Pubkey::new_unique()
        ),
        Err(TransactionError::InstructionError(
            0,
            InstructionError::InsufficientFunds
        )),
    );

    // Nothing moved but the fee.
    assert_eq!(bank.get_account(&programdata_address), Some(programdata));
    assert_eq!(bank.get_account(&buffer_address), Some(buffer));
    let fee = payer_lamports - bank.get_balance(&authority.pubkey());
    assert_eq!(
        bank.calculate_capitalization_for_tests(),
        capitalization - fee,
    );
    assert_eq!(
        bank.load_accounts_data_size_delta_on_chain(),
        data_size_delta
    );

    // The old program keeps running.
    let bank = next_bank(&bank_forks, bank);
    assert_eq!(
        invoke(&bank, &authority, &program_id),
        Err(TransactionError::InstructionError(
            0,
            InstructionError::Custom(1)
        ))
    );
    let env = current_env(&bank);
    assert_cache_contents(
        &bank,
        &program_id,
        &[ExpectedEntry {
            entry_type: EntryType::Loaded,
            deployment_slot: 0,
            environment: &env,
        }],
    );
}

#[test]
fn test_simd_0433_grow_and_shrink_across_slots() {
    let small_elf = read_test_elf("sbpfv3_return_err");
    let large_elf = read_test_elf("sbpfv3_return_ok");
    let GenesisConfigInfo {
        mut genesis_config,
        mint_keypair: authority,
        ..
    } = create_genesis_config(LAMPORTS_PER_SOL);
    genesis_config.rent = Rent::default();
    let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    assert!(
        bank.feature_set
            .is_active(&loader_v3_set_program_data_to_elf_length::id())
    );
    let program_id = create_program_with_elf(
        &bank,
        &bpf_loader_upgradeable::id(),
        &authority.pubkey(),
        &small_elf,
    );
    let bank = next_bank(&bank_forks, bank);

    // Grow.
    upgrade_and_assert_resized(&bank, &authority, &program_id, &large_elf);
    let grow_slot = bank.slot();

    // The grown program runs in the next slot, then shrinks.
    let bank = next_bank(&bank_forks, bank);
    assert_eq!(invoke(&bank, &authority, &program_id), Ok(()));
    upgrade_and_assert_resized(&bank, &authority, &program_id, &small_elf);
    let shrink_slot = bank.slot();

    // The shrunk program runs in the slot after.
    let bank = next_bank(&bank_forks, bank);
    assert_eq!(
        invoke(&bank, &authority, &program_id),
        Err(TransactionError::InstructionError(
            0,
            InstructionError::Custom(1)
        ))
    );
    let env = current_env(&bank);
    assert_cache_contents(
        &bank,
        &program_id,
        &[0, grow_slot, shrink_slot].map(|deployment_slot| ExpectedEntry {
            entry_type: EntryType::Loaded,
            deployment_slot,
            environment: &env,
        }),
    );
}
