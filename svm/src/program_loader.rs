#[cfg(feature = "metrics")]
use solana_program_runtime::loaded_programs::LoadProgramMetrics;
use {
    solana_account::{AccountSharedData, ReadableAccount, state_traits::StateMut},
    solana_clock::Slot,
    solana_loader_v3_interface::state::UpgradeableLoaderState,
    solana_loader_v4_interface::state::{LoaderV4State, LoaderV4Status},
    solana_program_runtime::loaded_programs::{
        DELAY_VISIBILITY_SLOT_OFFSET, ProgramCacheEntry, ProgramCacheEntryOwner,
        ProgramCacheEntryType, ProgramRuntimeEnvironments,
    },
    solana_pubkey::Pubkey,
    solana_sdk_ids::{bpf_loader, bpf_loader_deprecated, bpf_loader_upgradeable, loader_v4},
    solana_svm_callback::TransactionProcessingCallback,
    solana_svm_timings::ExecuteTimings,
    solana_svm_type_overrides::sync::Arc,
    solana_transaction_error::{TransactionError, TransactionResult},
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
        solana_loader_v4_program::get_state(program_account.data())
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
    environments: &ProgramRuntimeEnvironments,
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
            environments.program_runtime_v1.clone(),
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
            environments.program_runtime_v1.clone(),
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
                    environments.program_runtime_v1.clone(),
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
                        environments.program_runtime_v1.clone(),
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
        let env = environments.program_runtime_v1.clone();
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
    pubkey: &Pubkey,
) -> TransactionResult<Slot> {
    let (program, _slot) = callbacks
        .get_account_shared_data(pubkey)
        .ok_or(TransactionError::ProgramAccountNotFound)?;
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
        let state = solana_loader_v4_program::get_state(program.data())
            .map_err(|_| TransactionError::ProgramAccountNotFound)?;
        Ok(state.slot)
    } else {
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::transaction_processor::TransactionBatchProcessor,
        solana_account::WritableAccount,
        solana_program_runtime::{
            loaded_programs::{
                BlockRelation, ForkGraph, ProgramRuntimeEnvironment, ProgramRuntimeEnvironments,
            },
            solana_sbpf::program::BuiltinProgram,
        },
        solana_sdk_ids::{bpf_loader, bpf_loader_upgradeable},
        solana_svm_callback::InvokeContextCallback,
        std::{
            cell::RefCell,
            collections::HashMap,
            env,
            fs::{self, File},
            io::Read,
        },
    };

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
    fn test_load_program_accounts_loader_v4() {
        let key = Pubkey::new_unique();
        let mock_bank = MockBankCallback::default();
        let mut account_data = AccountSharedData::default();
        account_data.set_owner(loader_v4::id());
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key, (account_data.clone(), 0));

        let result = load_program_accounts(&mock_bank, &key);
        assert!(matches!(
            result,
            Some((ProgramAccountLoadResult::InvalidAccountData(_), _))
        ));

        account_data.set_data(vec![0; 64]);
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key, (account_data.clone(), 0));
        let result = load_program_accounts(&mock_bank, &key);
        assert!(matches!(
            result,
            Some((ProgramAccountLoadResult::InvalidAccountData(_), _))
        ));

        let loader_data = LoaderV4State {
            slot: 25,
            authority_address_or_next_version: Pubkey::new_unique(),
            status: LoaderV4Status::Deployed,
        };
        let encoded = unsafe {
            std::mem::transmute::<&LoaderV4State, &[u8; LoaderV4State::program_data_offset()]>(
                &loader_data,
            )
        };
        account_data.set_data(encoded.to_vec());
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key, (account_data.clone(), 25));

        let result = load_program_accounts(&mock_bank, &key);

        match result {
            Some((
                ProgramAccountLoadResult::ProgramOfLoaderV4(data, deployment_slot),
                last_modification_slot,
            )) => {
                assert_eq!(data, account_data);
                assert_eq!(deployment_slot, 25);
                assert_eq!(last_modification_slot, 25);
            }

            _ => panic!("Invalid result"),
        }
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
        let environment = ProgramRuntimeEnvironment::new(BuiltinProgram::new_mock());

        let result = ProgramCacheEntry::new(
            &loader,
            environment.clone(),
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
            &batch_processor.get_environments_for_epoch(50),
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
        account_data.set_owner(loader_v4::id());
        let batch_processor = TransactionBatchProcessor::<TestForkGraph>::default();
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key, (account_data.clone(), 0));

        let result = load_program_with_pubkey(
            &mock_bank,
            &batch_processor.get_environments_for_epoch(20),
            &key,
            0, // Slot 0
            &mut ExecuteTimings::default(),
        );

        let loaded_program = ProgramCacheEntry::new_tombstone(
            0, // Slot 0
            ProgramCacheEntryOwner::LoaderV4,
            ProgramCacheEntryType::FailedVerification(
                batch_processor
                    .get_environments_for_epoch(20)
                    .program_runtime_v1,
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
            &batch_processor.get_environments_for_epoch(20),
            &key,
            200,
            &mut ExecuteTimings::default(),
        );
        let loaded_program = ProgramCacheEntry::new_tombstone(
            0,
            ProgramCacheEntryOwner::LoaderV2,
            ProgramCacheEntryType::FailedVerification(
                batch_processor
                    .get_environments_for_epoch(20)
                    .program_runtime_v1,
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
            &batch_processor.get_environments_for_epoch(20),
            &key,
            200,
            &mut ExecuteTimings::default(),
        );

        let environments = ProgramRuntimeEnvironments::default();
        let expected = ProgramCacheEntry::new(
            account_data.owner(),
            environments.program_runtime_v1.clone(),
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
            &batch_processor.get_environments_for_epoch(0),
            &key1,
            0,
            &mut ExecuteTimings::default(),
        );
        let loaded_program = ProgramCacheEntry::new_tombstone(
            0,
            ProgramCacheEntryOwner::LoaderV3,
            ProgramCacheEntryType::FailedVerification(
                batch_processor
                    .get_environments_for_epoch(0)
                    .program_runtime_v1,
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
            &batch_processor.get_environments_for_epoch(20),
            &key1,
            200,
            &mut ExecuteTimings::default(),
        );

        let data = account_data.data();
        account_data
            .set_data(data[UpgradeableLoaderState::size_of_programdata_metadata()..].to_vec());

        let environments = ProgramRuntimeEnvironments::default();
        let expected = ProgramCacheEntry::new(
            account_data.owner(),
            environments.program_runtime_v1.clone(),
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
    fn test_load_program_of_loader_v4() {
        let key = Pubkey::new_unique();
        let mock_bank = MockBankCallback::default();
        let mut account_data = AccountSharedData::default();
        account_data.set_owner(loader_v4::id());
        let batch_processor = TransactionBatchProcessor::<TestForkGraph>::default();

        let loader_data = LoaderV4State {
            slot: 0,
            authority_address_or_next_version: Pubkey::new_unique(),
            status: LoaderV4Status::Deployed,
        };
        let encoded = unsafe {
            std::mem::transmute::<&LoaderV4State, &[u8; LoaderV4State::program_data_offset()]>(
                &loader_data,
            )
        };
        account_data.set_data(encoded.to_vec());
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key, (account_data.clone(), 0));

        let result = load_program_with_pubkey(
            &mock_bank,
            &batch_processor.get_environments_for_epoch(0),
            &key,
            0,
            &mut ExecuteTimings::default(),
        );
        let loaded_program = ProgramCacheEntry::new_tombstone(
            0,
            ProgramCacheEntryOwner::LoaderV4,
            ProgramCacheEntryType::FailedVerification(
                batch_processor
                    .get_environments_for_epoch(0)
                    .program_runtime_v1,
            ),
        );
        assert_eq!(result.unwrap(), (Arc::new(loaded_program), 0));

        let mut header = account_data.data().to_vec();
        let mut complement =
            vec![0; std::cmp::max(0, LoaderV4State::program_data_offset() - header.len())];
        header.append(&mut complement);

        let mut buffer = load_test_program();
        header.append(&mut buffer);

        account_data.set_data(header);
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key, (account_data.clone(), 0));

        let result = load_program_with_pubkey(
            &mock_bank,
            &batch_processor.get_environments_for_epoch(20),
            &key,
            200,
            &mut ExecuteTimings::default(),
        );

        let data = account_data.data()[LoaderV4State::program_data_offset()..].to_vec();
        account_data.set_data(data);
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key, (account_data.clone(), 0));

        let environments = ProgramRuntimeEnvironments::default();
        let expected = ProgramCacheEntry::new(
            account_data.owner(),
            environments.program_runtime_v1.clone(),
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
        let upcoming_environments = ProgramRuntimeEnvironments::default();
        let current_environments = batch_processor.environments.clone();
        {
            let mut epoch_boundary_preparation =
                batch_processor.epoch_boundary_preparation.write().unwrap();
            epoch_boundary_preparation.upcoming_epoch = 1;
            epoch_boundary_preparation.upcoming_environments = Some(upcoming_environments.clone());
        }
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key, (account_data.clone(), 0));

        for is_upcoming_env in [false, true] {
            let (result, _last_modification_slot) = load_program_with_pubkey(
                &mock_bank,
                &batch_processor.get_environments_for_epoch(is_upcoming_env as u64),
                &key,
                200,
                &mut ExecuteTimings::default(),
            )
            .unwrap();
            assert_ne!(
                is_upcoming_env,
                Arc::ptr_eq(
                    result.program.get_environment().unwrap(),
                    &current_environments.program_runtime_v1,
                )
            );
            assert_eq!(
                is_upcoming_env,
                Arc::ptr_eq(
                    result.program.get_environment().unwrap(),
                    &upcoming_environments.program_runtime_v1,
                )
            );
        }
    }

    #[test]
    fn test_program_modification_slot_account_not_found() {
        let mock_bank = MockBankCallback::default();

        let key = Pubkey::new_unique();

        let result = get_program_deployment_slot(&mock_bank, &key);
        assert_eq!(result.err(), Some(TransactionError::ProgramAccountNotFound));

        let mut account_data = AccountSharedData::new(100, 100, &bpf_loader_upgradeable::id());
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key, (account_data.clone(), 0));

        let result = get_program_deployment_slot(&mock_bank, &key);
        assert_eq!(result.err(), Some(TransactionError::ProgramAccountNotFound));

        let state = UpgradeableLoaderState::Program {
            programdata_address: Pubkey::new_unique(),
        };
        account_data.set_data(bincode::serialize(&state).unwrap());
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key, (account_data.clone(), 0));

        let result = get_program_deployment_slot(&mock_bank, &key);
        assert_eq!(result.err(), Some(TransactionError::ProgramAccountNotFound));

        account_data.set_owner(loader_v4::id());
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key, (account_data, 0));

        let result = get_program_deployment_slot(&mock_bank, &key);
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

        let account_data = AccountSharedData::new_data(
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
            .insert(key2, (account_data, 0));

        let result = get_program_deployment_slot(&mock_bank, &key1);
        assert_eq!(result.unwrap(), 77);

        let state = LoaderV4State {
            slot: 58,
            authority_address_or_next_version: Pubkey::new_unique(),
            status: LoaderV4Status::Deployed,
        };
        let encoded = unsafe {
            std::mem::transmute::<&LoaderV4State, &[u8; LoaderV4State::program_data_offset()]>(
                &state,
            )
        };
        let mut account_data = AccountSharedData::new(100, encoded.len(), &loader_v4::id());
        account_data.set_data(encoded.to_vec());
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key1, (account_data.clone(), 0));

        let result = get_program_deployment_slot(&mock_bank, &key1);
        assert_eq!(result.unwrap(), 58);

        account_data.set_owner(Pubkey::new_unique());
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key2, (account_data, 0));

        let result = get_program_deployment_slot(&mock_bank, &key2);
        assert_eq!(result.unwrap(), 0);
    }
}
