#[cfg(feature = "metrics")]
use solana_program_runtime::program_metrics::LoadProgramMetrics;
use {
    crate::account_loader::PROGRAM_OWNERS,
    solana_account::{AccountSharedData, ReadableAccount, state_traits::StateMut},
    solana_clock::Slot,
    solana_instruction::error::InstructionError,
    solana_loader_v3_interface::state::UpgradeableLoaderState,
    solana_loader_v4_interface::state::{LoaderV4State, LoaderV4Status},
    solana_program_runtime::{
        loaded_programs::{
            ProgramCacheForTxBatch, ProgramCacheMatchCriteria, ProgramRuntimeEnvironment,
        },
        program_cache_entry::{
            DELAY_VISIBILITY_SLOT_OFFSET, ProgramCacheEntry, ProgramCacheEntryOwner,
            ProgramCacheEntryType,
        },
    },
    solana_pubkey::Pubkey,
    solana_sdk_ids::{bpf_loader, bpf_loader_deprecated, bpf_loader_upgradeable, loader_v4},
    solana_svm_callback::TransactionProcessingCallback,
    solana_svm_timings::ExecuteTimings,
    solana_svm_transaction::svm_message::SVMMessage,
    solana_svm_type_overrides::sync::Arc,
    solana_transaction_error::{TransactionError, TransactionResult},
    std::{collections::HashMap, sync::atomic::Ordering},
};

#[derive(Debug)]
pub(crate) enum ProgramAccountLoadResult {
    InvalidAccountData(ProgramCacheEntryOwner),
    ProgramOfLoaderV1(AccountSharedData),
    ProgramOfLoaderV2(AccountSharedData),
    ProgramOfLoaderV3(AccountSharedData, AccountSharedData, Slot),
    ProgramOfLoaderV4(AccountSharedData, Slot),
}

pub(crate) fn load_program_accounts<CB: TransactionProcessingCallback>(
    callbacks: &CB,
    pubkey: &Pubkey,
) -> Option<(ProgramAccountLoadResult, Slot)> {
    let (program_account, last_modification_slot) = callbacks.get_account_shared_data(pubkey)?;

    let load_result = if loader_v4::check_id(program_account.owner()) {
        loader_v4_get_state(program_account.data())
            .ok()
            .and_then(|state| {
                (!matches!(state.status, LoaderV4Status::Retracted)).then_some(state.slot)
            })
            .map(|slot| ProgramAccountLoadResult::ProgramOfLoaderV4(program_account, slot))
            .unwrap_or(ProgramAccountLoadResult::InvalidAccountData(
                ProgramCacheEntryOwner::LoaderV4,
            ))
    } else if bpf_loader_upgradeable::check_id(program_account.owner()) {
        if let Ok(UpgradeableLoaderState::Program {
            programdata_address,
        }) = program_account.state()
        {
            if let Some((programdata_account, _slot)) =
                callbacks.get_account_shared_data(&programdata_address)
            {
                if bpf_loader_upgradeable::check_id(programdata_account.owner()) {
                    if let Ok(UpgradeableLoaderState::ProgramData {
                        slot,
                        upgrade_authority_address: _,
                    }) = programdata_account.state()
                    {
                        ProgramAccountLoadResult::ProgramOfLoaderV3(
                            program_account,
                            programdata_account,
                            slot,
                        )
                    } else {
                        ProgramAccountLoadResult::InvalidAccountData(
                            ProgramCacheEntryOwner::LoaderV3,
                        )
                    }
                } else {
                    ProgramAccountLoadResult::InvalidAccountData(ProgramCacheEntryOwner::LoaderV3)
                }
            } else {
                ProgramAccountLoadResult::InvalidAccountData(ProgramCacheEntryOwner::LoaderV3)
            }
        } else {
            ProgramAccountLoadResult::InvalidAccountData(ProgramCacheEntryOwner::LoaderV3)
        }
    } else if bpf_loader::check_id(program_account.owner()) {
        ProgramAccountLoadResult::ProgramOfLoaderV2(program_account)
    } else if bpf_loader_deprecated::check_id(program_account.owner()) {
        ProgramAccountLoadResult::ProgramOfLoaderV1(program_account)
    } else {
        return None;
    };

    Some((load_result, last_modification_slot))
}

/// Loads the program with the given pubkey.
///
/// If the account doesn't exist it returns `None`. If the account does exist, it must be a program
/// account (belong to one of the program loaders). Returns `Some(InvalidAccountData)` if the program
/// account is `Closed`, contains invalid data or any of the programdata accounts are invalid.
pub fn load_program_with_pubkey<CB: TransactionProcessingCallback>(
    callbacks: &CB,
    program_runtime_environment: &ProgramRuntimeEnvironment,
    pubkey: &Pubkey,
    current_slot: Slot,
    execute_timings: &mut ExecuteTimings,
) -> Option<(Arc<ProgramCacheEntry>, Slot)> {
    #[cfg(feature = "metrics")]
    let mut load_program_metrics = LoadProgramMetrics {
        program_id: pubkey.to_string(),
        ..LoadProgramMetrics::default()
    };
    #[cfg(not(feature = "metrics"))]
    let _ = execute_timings;

    let (load_result, last_modification_slot) = load_program_accounts(callbacks, pubkey)?;
    let loaded_program = match load_result {
        ProgramAccountLoadResult::InvalidAccountData(owner) => Ok(
            ProgramCacheEntry::new_tombstone(current_slot, owner, ProgramCacheEntryType::Closed),
        ),

        ProgramAccountLoadResult::ProgramOfLoaderV1(program_account) => ProgramCacheEntry::new(
            program_account.owner(),
            ProgramRuntimeEnvironment::clone(program_runtime_environment),
            0,
            DELAY_VISIBILITY_SLOT_OFFSET,
            program_account.data(),
            program_account.data().len(),
            #[cfg(feature = "metrics")]
            &mut load_program_metrics,
        )
        .map_err(|_| (0, ProgramCacheEntryOwner::LoaderV1)),

        ProgramAccountLoadResult::ProgramOfLoaderV2(program_account) => ProgramCacheEntry::new(
            program_account.owner(),
            ProgramRuntimeEnvironment::clone(program_runtime_environment),
            0,
            DELAY_VISIBILITY_SLOT_OFFSET,
            program_account.data(),
            program_account.data().len(),
            #[cfg(feature = "metrics")]
            &mut load_program_metrics,
        )
        .map_err(|_| (0, ProgramCacheEntryOwner::LoaderV2)),

        ProgramAccountLoadResult::ProgramOfLoaderV3(
            program_account,
            programdata_account,
            deployment_slot,
        ) => programdata_account
            .data()
            .get(UpgradeableLoaderState::size_of_programdata_metadata()..)
            .ok_or(())
            .and_then(|programdata| {
                ProgramCacheEntry::new(
                    program_account.owner(),
                    ProgramRuntimeEnvironment::clone(program_runtime_environment),
                    deployment_slot,
                    deployment_slot.saturating_add(DELAY_VISIBILITY_SLOT_OFFSET),
                    programdata,
                    program_account
                        .data()
                        .len()
                        .saturating_add(programdata_account.data().len()),
                    #[cfg(feature = "metrics")]
                    &mut load_program_metrics,
                )
                .map_err(|_| ())
            })
            .map_err(|_| (deployment_slot, ProgramCacheEntryOwner::LoaderV3)),

        ProgramAccountLoadResult::ProgramOfLoaderV4(program_account, deployment_slot) => {
            program_account
                .data()
                .get(LoaderV4State::program_data_offset()..)
                .ok_or(())
                .and_then(|elf_bytes| {
                    ProgramCacheEntry::new(
                        &loader_v4::id(),
                        ProgramRuntimeEnvironment::clone(program_runtime_environment),
                        deployment_slot,
                        deployment_slot.saturating_add(DELAY_VISIBILITY_SLOT_OFFSET),
                        elf_bytes,
                        program_account.data().len(),
                        #[cfg(feature = "metrics")]
                        &mut load_program_metrics,
                    )
                    .map_err(|_| ())
                })
                .map_err(|_| (deployment_slot, ProgramCacheEntryOwner::LoaderV4))
        }
    }
    .unwrap_or_else(|(deployment_slot, owner)| {
        let env = ProgramRuntimeEnvironment::clone(program_runtime_environment);
        ProgramCacheEntry::new_tombstone(
            deployment_slot,
            owner,
            ProgramCacheEntryType::FailedVerification(env),
        )
    });

    #[cfg(feature = "metrics")]
    load_program_metrics.submit_datapoint(&mut execute_timings.details);
    loaded_program.update_access_slot(current_slot);
    Some((Arc::new(loaded_program), last_modification_slot))
}

/// Find the slot in which the program was most recently re-/deployed.
/// Returns slot 0 for programs deployed with v1/v2 loaders, since programs deployed
/// with those loaders do not retain deployment slot information.
/// Returns an error if the program's account state can not be found or parsed.
pub(crate) fn get_program_deployment_slot<CB: TransactionProcessingCallback>(
    callbacks: &CB,
    program: &AccountSharedData,
) -> TransactionResult<Slot> {
    if bpf_loader_upgradeable::check_id(program.owner()) {
        if let Ok(UpgradeableLoaderState::Program {
            programdata_address,
        }) = program.state()
        {
            let (programdata, _slot) = callbacks
                .get_account_shared_data(&programdata_address)
                .ok_or(TransactionError::ProgramAccountNotFound)?;
            if let Ok(UpgradeableLoaderState::ProgramData {
                slot,
                upgrade_authority_address: _,
            }) = programdata.state()
            {
                return Ok(slot);
            }
        }
        Err(TransactionError::ProgramAccountNotFound)
    } else if loader_v4::check_id(program.owner()) {
        let state = loader_v4_get_state(program.data())
            .map_err(|_| TransactionError::ProgramAccountNotFound)?;
        Ok(state.slot)
    } else {
        Ok(0)
    }
}

/// Appends to a set of executable program accounts (all accounts owned by any loader)
/// for transactions with a valid blockhash or nonce.
pub fn filter_executable_program_accounts<CB: TransactionProcessingCallback>(
    callbacks: &CB,
    program_cache_for_tx_batch: &mut ProgramCacheForTxBatch,
    tx: &impl SVMMessage,
    check_program_deployment_slot: bool,
) -> Vec<(Pubkey, ProgramCacheMatchCriteria, Slot)> {
    let mut program_accounts_set: HashMap<Pubkey, (ProgramCacheMatchCriteria, Slot)> =
        HashMap::default();
    for account_key in tx.account_keys().iter() {
        if let Some(cache_entry) = program_cache_for_tx_batch.find(account_key) {
            cache_entry.stats.uses.fetch_add(1, Ordering::Relaxed);
        } else if let Some((account, last_modification_slot)) =
            callbacks.get_account_shared_data(account_key)
            && PROGRAM_OWNERS.contains(account.owner())
        {
            let match_criteria = if check_program_deployment_slot {
                get_program_deployment_slot(callbacks, &account)
                    .map_or(ProgramCacheMatchCriteria::Tombstone, |slot| {
                        ProgramCacheMatchCriteria::DeployedOnOrAfterSlot(slot)
                    })
            } else {
                ProgramCacheMatchCriteria::NoCriteria
            };
            program_accounts_set.insert(*account_key, (match_criteria, last_modification_slot));
        }
    }
    program_accounts_set
        .iter()
        .map(|(pubkey, (match_criteria, last_modification_slot))| {
            (*pubkey, *match_criteria, *last_modification_slot)
        })
        .collect()
}

// Plucked from the now-removed Loader V4 program library.
fn loader_v4_get_state(data: &[u8]) -> Result<&LoaderV4State, InstructionError> {
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

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::transaction_processor::TransactionBatchProcessor,
        solana_account::WritableAccount,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_message::{
            LegacyMessage, Message, MessageHeader, SanitizedMessage,
            compiled_instruction::CompiledInstruction,
        },
        solana_program_runtime::{
            loaded_programs::{
                BlockRelation, ForkGraph, ProgramRuntimeEnvironment,
                get_mock_program_runtime_environment,
            },
            solana_sbpf::program::BuiltinProgram,
        },
        solana_sdk_ids::{bpf_loader, bpf_loader_upgradeable},
        solana_signature::Signature,
        solana_svm_callback::InvokeContextCallback,
        solana_transaction::{Transaction, sanitized::SanitizedTransaction},
        std::{
            cell::RefCell,
            collections::{HashMap, HashSet},
            env,
            fs::{self, File},
            io::Read,
        },
    };

    fn new_unchecked_sanitized_message(message: Message) -> SanitizedMessage {
        SanitizedMessage::Legacy(LegacyMessage::new(message, &HashSet::new()))
    }

    struct TestForkGraph {}

    impl ForkGraph for TestForkGraph {
        fn relationship(&self, _a: Slot, _b: Slot) -> BlockRelation {
            BlockRelation::Unknown
        }
    }

    #[derive(Default, Clone)]
    pub(crate) struct MockBankCallback {
        pub(crate) account_shared_data: RefCell<HashMap<Pubkey, (AccountSharedData, Slot)>>,
    }

    impl InvokeContextCallback for MockBankCallback {}

    impl TransactionProcessingCallback for MockBankCallback {
        fn get_account_shared_data(&self, pubkey: &Pubkey) -> Option<(AccountSharedData, Slot)> {
            self.account_shared_data.borrow().get(pubkey).cloned()
        }
    }

    #[test]
    fn test_load_program_accounts_account_not_found() {
        let mock_bank = MockBankCallback::default();
        let key = Pubkey::new_unique();

        let result = load_program_accounts(&mock_bank, &key);
        assert!(result.is_none());

        let mut account_data = AccountSharedData::default();
        account_data.set_owner(bpf_loader_upgradeable::id());
        let state = UpgradeableLoaderState::Program {
            programdata_address: Pubkey::new_unique(),
        };
        account_data.set_data(bincode::serialize(&state).unwrap());
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key, (account_data.clone(), 0));

        let result = load_program_accounts(&mock_bank, &key);
        assert!(matches!(
            result,
            Some((ProgramAccountLoadResult::InvalidAccountData(_), _))
        ));

        account_data.set_data(Vec::new());
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key, (account_data, 0));

        let result = load_program_accounts(&mock_bank, &key);

        assert!(matches!(
            result,
            Some((ProgramAccountLoadResult::InvalidAccountData(_), _))
        ));
    }

    #[test]
    fn test_load_program_accounts_loader_v1_or_v2() {
        let key = Pubkey::new_unique();
        let mock_bank = MockBankCallback::default();
        let mut account_data = AccountSharedData::default();
        account_data.set_owner(bpf_loader::id());
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key, (account_data.clone(), 0));

        let result = load_program_accounts(&mock_bank, &key);
        match result {
            Some((ProgramAccountLoadResult::ProgramOfLoaderV1(data), last_modification_slot))
            | Some((ProgramAccountLoadResult::ProgramOfLoaderV2(data), last_modification_slot)) => {
                assert_eq!(data, account_data);
                assert_eq!(last_modification_slot, 0);
            }
            _ => panic!("Invalid result"),
        }
    }

    #[test]
    fn test_load_program_accounts_success() {
        let key1 = Pubkey::new_unique();
        let key2 = Pubkey::new_unique();
        let mock_bank = MockBankCallback::default();

        let mut account_data = AccountSharedData::default();
        account_data.set_owner(bpf_loader_upgradeable::id());

        let state = UpgradeableLoaderState::Program {
            programdata_address: key2,
        };
        account_data.set_data(bincode::serialize(&state).unwrap());
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key1, (account_data.clone(), 25));

        let state = UpgradeableLoaderState::ProgramData {
            slot: 25,
            upgrade_authority_address: None,
        };
        let mut account_data2 = AccountSharedData::default();
        account_data2.set_owner(bpf_loader_upgradeable::id());
        account_data2.set_data(bincode::serialize(&state).unwrap());
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key2, (account_data2.clone(), 25));

        let result = load_program_accounts(&mock_bank, &key1);

        match result {
            Some((
                ProgramAccountLoadResult::ProgramOfLoaderV3(data1, data2, deployment_slot),
                last_modification_slot,
            )) => {
                assert_eq!(data1, account_data);
                assert_eq!(data2, account_data2);
                assert_eq!(deployment_slot, 25);
                assert_eq!(last_modification_slot, 25);
            }

            _ => panic!("Invalid result"),
        }
    }

    fn load_test_program() -> Vec<u8> {
        let mut dir = env::current_dir().unwrap();
        dir.push("tests");
        dir.push("example-programs");
        dir.push("hello-solana");
        dir.push("hello_solana_program.so");
        let mut file = File::open(dir.clone()).expect("file not found");
        let metadata = fs::metadata(dir).expect("Unable to read metadata");
        let mut buffer = vec![0; metadata.len() as usize];
        file.read_exact(&mut buffer).expect("Buffer overflow");
        buffer
    }

    #[test]
    fn test_load_program_from_bytes() {
        let buffer = load_test_program();

        #[cfg(feature = "metrics")]
        let mut metrics = LoadProgramMetrics::default();
        let loader = bpf_loader_upgradeable::id();
        let size = buffer.len();
        let slot: Slot = 2;
        let environment = ProgramRuntimeEnvironment::from(BuiltinProgram::new_mock());

        let result = ProgramCacheEntry::new(
            &loader,
            ProgramRuntimeEnvironment::clone(&environment),
            slot,
            slot.saturating_add(DELAY_VISIBILITY_SLOT_OFFSET),
            &buffer,
            size,
            #[cfg(feature = "metrics")]
            &mut metrics,
        );

        assert!(result.is_ok());
    }

    #[test]
    fn test_load_program_not_found() {
        let mock_bank = MockBankCallback::default();
        let key = Pubkey::new_unique();
        let batch_processor = TransactionBatchProcessor::<TestForkGraph>::default();

        let result = load_program_with_pubkey(
            &mock_bank,
            &batch_processor.program_runtime_environment_for_epoch(50),
            &key,
            500,
            &mut ExecuteTimings::default(),
        );
        assert!(result.is_none());
    }

    #[test]
    fn test_load_program_invalid_account_data() {
        let key = Pubkey::new_unique();
        let mock_bank = MockBankCallback::default();
        let mut account_data = AccountSharedData::default();
        account_data.set_owner(bpf_loader_upgradeable::id());
        let batch_processor = TransactionBatchProcessor::<TestForkGraph>::default();
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key, (account_data.clone(), 0));

        let result = load_program_with_pubkey(
            &mock_bank,
            &batch_processor.program_runtime_environment_for_epoch(20),
            &key,
            0, // Slot 0
            &mut ExecuteTimings::default(),
        );

        let loaded_program = ProgramCacheEntry::new_tombstone(
            0, // Slot 0
            ProgramCacheEntryOwner::LoaderV3,
            ProgramCacheEntryType::FailedVerification(
                batch_processor.program_runtime_environment_for_epoch(20),
            ),
        );
        assert_eq!(result.unwrap(), (Arc::new(loaded_program), 0));
    }

    #[test]
    fn test_load_program_program_loader_v1_or_v2() {
        let key = Pubkey::new_unique();
        let mock_bank = MockBankCallback::default();
        let mut account_data = AccountSharedData::default();
        account_data.set_owner(bpf_loader::id());
        let batch_processor = TransactionBatchProcessor::<TestForkGraph>::default();
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key, (account_data.clone(), 0));

        // This should return an error
        let result = load_program_with_pubkey(
            &mock_bank,
            &batch_processor.program_runtime_environment_for_epoch(20),
            &key,
            200,
            &mut ExecuteTimings::default(),
        );
        let loaded_program = ProgramCacheEntry::new_tombstone(
            0,
            ProgramCacheEntryOwner::LoaderV2,
            ProgramCacheEntryType::FailedVerification(
                batch_processor.program_runtime_environment_for_epoch(20),
            ),
        );
        assert_eq!(result.unwrap(), (Arc::new(loaded_program), 0));

        let buffer = load_test_program();
        account_data.set_data(buffer);

        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key, (account_data.clone(), 0));

        let result = load_program_with_pubkey(
            &mock_bank,
            &batch_processor.program_runtime_environment_for_epoch(20),
            &key,
            200,
            &mut ExecuteTimings::default(),
        );

        let program_runtime_environment = get_mock_program_runtime_environment();
        let expected = ProgramCacheEntry::new(
            account_data.owner(),
            ProgramRuntimeEnvironment::clone(&program_runtime_environment),
            0,
            DELAY_VISIBILITY_SLOT_OFFSET,
            account_data.data(),
            account_data.data().len(),
            #[cfg(feature = "metrics")]
            &mut LoadProgramMetrics::default(),
        );

        assert_eq!(result.unwrap(), (Arc::new(expected.unwrap()), 0));
    }

    #[test]
    fn test_load_program_program_loader_v3() {
        let key1 = Pubkey::new_unique();
        let key2 = Pubkey::new_unique();
        let mock_bank = MockBankCallback::default();
        let batch_processor = TransactionBatchProcessor::<TestForkGraph>::default();

        let mut account_data = AccountSharedData::default();
        account_data.set_owner(bpf_loader_upgradeable::id());

        let state = UpgradeableLoaderState::Program {
            programdata_address: key2,
        };
        account_data.set_data(bincode::serialize(&state).unwrap());
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key1, (account_data.clone(), 0));

        let state = UpgradeableLoaderState::ProgramData {
            slot: 0,
            upgrade_authority_address: None,
        };
        let mut account_data2 = AccountSharedData::default();
        account_data2.set_data(bincode::serialize(&state).unwrap());
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key2, (account_data2.clone(), 0));

        // This should return an error
        let result = load_program_with_pubkey(
            &mock_bank,
            &batch_processor.program_runtime_environment_for_epoch(0),
            &key1,
            0,
            &mut ExecuteTimings::default(),
        );
        let loaded_program = ProgramCacheEntry::new_tombstone(
            0,
            ProgramCacheEntryOwner::LoaderV3,
            ProgramCacheEntryType::FailedVerification(
                batch_processor.program_runtime_environment_for_epoch(0),
            ),
        );
        assert_eq!(result.unwrap(), (Arc::new(loaded_program), 0));

        let mut buffer = load_test_program();
        let mut header = bincode::serialize(&state).unwrap();
        let mut complement = vec![
            0;
            std::cmp::max(
                0,
                UpgradeableLoaderState::size_of_programdata_metadata() - header.len()
            )
        ];
        header.append(&mut complement);
        header.append(&mut buffer);
        account_data.set_data(header);

        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key2, (account_data.clone(), 0));

        let result = load_program_with_pubkey(
            &mock_bank,
            &batch_processor.program_runtime_environment_for_epoch(20),
            &key1,
            200,
            &mut ExecuteTimings::default(),
        );

        let data = account_data.data();
        account_data
            .set_data(data[UpgradeableLoaderState::size_of_programdata_metadata()..].to_vec());

        let program_runtime_environment = get_mock_program_runtime_environment();
        let expected = ProgramCacheEntry::new(
            account_data.owner(),
            ProgramRuntimeEnvironment::clone(&program_runtime_environment),
            0,
            DELAY_VISIBILITY_SLOT_OFFSET,
            account_data.data(),
            account_data.data().len(),
            #[cfg(feature = "metrics")]
            &mut LoadProgramMetrics::default(),
        );
        assert_eq!(result.unwrap(), (Arc::new(expected.unwrap()), 0));
    }

    #[test]
    fn test_load_program_environment() {
        let key = Pubkey::new_unique();
        let mock_bank = MockBankCallback::default();
        let mut account_data = AccountSharedData::default();
        account_data.set_owner(bpf_loader::id());
        let batch_processor = TransactionBatchProcessor::<TestForkGraph>::default();
        let upcoming_environment = get_mock_program_runtime_environment();
        let current_environment =
            ProgramRuntimeEnvironment::clone(&batch_processor.program_runtime_environment);
        {
            let mut epoch_boundary_preparation =
                batch_processor.epoch_boundary_preparation.write().unwrap();
            epoch_boundary_preparation.upcoming_epoch = 1;
            epoch_boundary_preparation.upcoming_environment = Some(upcoming_environment.clone());
        }
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key, (account_data.clone(), 0));

        for is_upcoming_env in [false, true] {
            let (result, _last_modification_slot) = load_program_with_pubkey(
                &mock_bank,
                &batch_processor.program_runtime_environment_for_epoch(is_upcoming_env as u64),
                &key,
                200,
                &mut ExecuteTimings::default(),
            )
            .unwrap();
            assert_ne!(
                is_upcoming_env,
                result.program.get_environment().unwrap() == &current_environment,
            );
            assert_eq!(
                is_upcoming_env,
                result.program.get_environment().unwrap() == &upcoming_environment,
            );
        }
    }

    #[test]
    fn test_program_modification_slot_account_not_found() {
        let mock_bank = MockBankCallback::default();
        let key = Pubkey::new_unique();

        let mut account_data = AccountSharedData::new(100, 100, &bpf_loader_upgradeable::id());
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key, (account_data.clone(), 0));

        let result = get_program_deployment_slot(
            &mock_bank,
            &mock_bank.get_account_shared_data(&key).unwrap().0,
        );
        assert_eq!(result.err(), Some(TransactionError::ProgramAccountNotFound));

        let state = UpgradeableLoaderState::Program {
            programdata_address: Pubkey::new_unique(),
        };
        account_data.set_data(bincode::serialize(&state).unwrap());
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key, (account_data.clone(), 0));

        let result = get_program_deployment_slot(
            &mock_bank,
            &mock_bank.get_account_shared_data(&key).unwrap().0,
        );
        assert_eq!(result.err(), Some(TransactionError::ProgramAccountNotFound));
    }

    #[test]
    fn test_program_deployment_slot_success() {
        let mock_bank = MockBankCallback::default();

        let key1 = Pubkey::new_unique();
        let key2 = Pubkey::new_unique();

        let account_data = AccountSharedData::new_data(
            100,
            &UpgradeableLoaderState::Program {
                programdata_address: key2,
            },
            &bpf_loader_upgradeable::id(),
        )
        .unwrap();
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key1, (account_data, 0));

        let mut account_data = AccountSharedData::new_data(
            100,
            &UpgradeableLoaderState::ProgramData {
                slot: 77,
                upgrade_authority_address: None,
            },
            &bpf_loader_upgradeable::id(),
        )
        .unwrap();
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key2, (account_data.clone(), 0));

        let result = get_program_deployment_slot(
            &mock_bank,
            &mock_bank.get_account_shared_data(&key1).unwrap().0,
        );
        assert_eq!(result.unwrap(), 77);

        account_data.set_owner(Pubkey::new_unique());
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key2, (account_data, 0));

        let result = get_program_deployment_slot(
            &mock_bank,
            &mock_bank.get_account_shared_data(&key2).unwrap().0,
        );
        assert_eq!(result.unwrap(), 0);
    }

    #[test]
    fn test_filter_executable_program_accounts() {
        let mock_bank = MockBankCallback::default();
        let key1 = Pubkey::new_unique();
        let owner1 = bpf_loader::id();
        let key2 = Pubkey::new_unique();
        let owner2 = bpf_loader_upgradeable::id();

        let mut data1 = AccountSharedData::default();
        data1.set_owner(owner1);
        data1.set_lamports(93);
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key1, (data1, 0));

        let mut data2 = AccountSharedData::default();
        data2.set_owner(owner2);
        data2.set_lamports(90);
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key2, (data2, 0));

        let message = Message {
            account_keys: vec![key1, key2],
            header: MessageHeader::default(),
            instructions: vec![CompiledInstruction {
                program_id_index: 0,
                accounts: vec![],
                data: vec![],
            }],
            recent_blockhash: Hash::default(),
        };

        let sanitized_message = new_unchecked_sanitized_message(message);

        let sanitized_transaction = SanitizedTransaction::new_for_tests(
            sanitized_message,
            vec![Signature::new_unique()],
            false,
        );

        let missing_programs = filter_executable_program_accounts(
            &mock_bank,
            &mut ProgramCacheForTxBatch::default(),
            &sanitized_transaction,
            false,
        );
        assert_eq!(missing_programs.len(), 2);
        assert!(
            missing_programs
                .iter()
                .position(|(key, _, _)| key == &key1)
                .is_some()
        );
        assert!(
            missing_programs
                .iter()
                .position(|(key, _, _)| key == &key2)
                .is_some()
        );
    }

    #[test]
    fn test_filter_executable_program_accounts_no_errors() {
        let keypair1 = Keypair::new();
        let keypair2 = Keypair::new();

        let non_program_pubkey1 = Pubkey::new_unique();
        let non_program_pubkey2 = Pubkey::new_unique();
        let program1_pubkey = bpf_loader::id();
        let program2_pubkey = bpf_loader_upgradeable::id();
        let account1_pubkey = Pubkey::new_unique();
        let account2_pubkey = Pubkey::new_unique();
        let account3_pubkey = Pubkey::new_unique();
        let account4_pubkey = Pubkey::new_unique();

        let account5_pubkey = Pubkey::new_unique();

        let mock_bank = MockBankCallback::default();
        mock_bank.account_shared_data.borrow_mut().insert(
            non_program_pubkey1,
            (AccountSharedData::new(1, 10, &account5_pubkey), 0),
        );
        mock_bank.account_shared_data.borrow_mut().insert(
            non_program_pubkey2,
            (AccountSharedData::new(1, 10, &account5_pubkey), 0),
        );
        mock_bank.account_shared_data.borrow_mut().insert(
            program1_pubkey,
            (AccountSharedData::new(40, 1, &account5_pubkey), 0),
        );
        mock_bank.account_shared_data.borrow_mut().insert(
            program2_pubkey,
            (AccountSharedData::new(40, 1, &account5_pubkey), 0),
        );
        mock_bank.account_shared_data.borrow_mut().insert(
            account1_pubkey,
            (AccountSharedData::new(1, 10, &non_program_pubkey1), 0),
        );
        mock_bank.account_shared_data.borrow_mut().insert(
            account2_pubkey,
            (AccountSharedData::new(1, 10, &non_program_pubkey2), 0),
        );
        mock_bank.account_shared_data.borrow_mut().insert(
            account3_pubkey,
            (AccountSharedData::new(40, 1, &program1_pubkey), 0),
        );
        mock_bank.account_shared_data.borrow_mut().insert(
            account4_pubkey,
            (AccountSharedData::new(40, 1, &program2_pubkey), 0),
        );

        let tx1 = Transaction::new_with_compiled_instructions(
            &[&keypair1],
            &[non_program_pubkey1],
            Hash::new_unique(),
            vec![account1_pubkey, account2_pubkey, account3_pubkey],
            vec![CompiledInstruction::new(1, &(), vec![0])],
        );
        let sanitized_tx1 = SanitizedTransaction::from_transaction_for_tests(tx1);

        let tx2 = Transaction::new_with_compiled_instructions(
            &[&keypair2],
            &[non_program_pubkey2],
            Hash::new_unique(),
            vec![account4_pubkey, account3_pubkey, account2_pubkey],
            vec![CompiledInstruction::new(1, &(), vec![0])],
        );
        let sanitized_tx2 = SanitizedTransaction::from_transaction_for_tests(tx2);

        let missing_programs = filter_executable_program_accounts(
            &mock_bank,
            &mut ProgramCacheForTxBatch::default(),
            &sanitized_tx1,
            false,
        );
        assert_eq!(missing_programs.len(), 1);
        assert!(
            missing_programs
                .iter()
                .position(|(key, _, _)| key == &account3_pubkey)
                .is_some()
        );

        let missing_programs = filter_executable_program_accounts(
            &mock_bank,
            &mut ProgramCacheForTxBatch::default(),
            &sanitized_tx2,
            false,
        );
        assert_eq!(missing_programs.len(), 2);
        assert!(
            missing_programs
                .iter()
                .position(|(key, _, _)| key == &account3_pubkey)
                .is_some()
        );
        assert!(
            missing_programs
                .iter()
                .position(|(key, _, _)| key == &account4_pubkey)
                .is_some()
        );
    }
}
