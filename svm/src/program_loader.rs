#[cfg(feature = "metrics")]
use solana_program_runtime::program_metrics::LoadProgramMetrics;
use {
    solana_account::{AccountSharedData, ReadableAccount},
    solana_clock::Slot,
    solana_instruction::error::InstructionError,
    solana_loader_v3_interface::state::UpgradeableLoaderState,
    solana_loader_v4_interface::state::{LoaderV4State, LoaderV4Status},
    solana_program_runtime::{
        loaded_programs::{ProgramCacheForTxBatch, ProgramRuntimeEnvironment, ProgramToLoad},
        program_cache_entry::{ProgramCacheEntry, ProgramCacheEntryOwner},
    },
    solana_pubkey::Pubkey,
    solana_sdk_ids::{bpf_loader, bpf_loader_deprecated, bpf_loader_upgradeable, loader_v4},
    solana_svm_callback::TransactionProcessingCallback,
    solana_svm_timings::ExecuteTimings,
    solana_svm_type_overrides::sync::Arc,
    solana_transaction_error::{TransactionError, TransactionResult},
    std::sync::atomic::Ordering,
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
) -> Option<ProgramAccountLoadResult> {
    let (program_account, _last_modification_slot) = callbacks.get_account_shared_data(pubkey)?;

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
        }) = bincode::deserialize(program_account.data())
        {
            if let Some((programdata_account, _slot)) =
                callbacks.get_account_shared_data(&programdata_address)
            {
                if bpf_loader_upgradeable::check_id(programdata_account.owner()) {
                    if let Ok(UpgradeableLoaderState::ProgramData {
                        slot,
                        upgrade_authority_address: _,
                    }) = bincode::deserialize(programdata_account.data())
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

    Some(load_result)
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
) -> Option<Arc<ProgramCacheEntry>> {
    #[cfg(feature = "metrics")]
    let mut load_program_metrics = LoadProgramMetrics {
        program_id: pubkey.to_string(),
        ..LoadProgramMetrics::default()
    };
    #[cfg(not(feature = "metrics"))]
    let _ = execute_timings;

    let load_result = load_program_accounts(callbacks, pubkey)?;
    let loaded_program = match load_result {
        ProgramAccountLoadResult::InvalidAccountData(owner) => {
            Ok(ProgramCacheEntry::new_closed_tombstone(current_slot, owner))
        }

        ProgramAccountLoadResult::ProgramOfLoaderV1(program_account) => ProgramCacheEntry::load(
            program_account.owner(),
            ProgramRuntimeEnvironment::clone(program_runtime_environment),
            0,
            program_account.data(),
            #[cfg(feature = "metrics")]
            &mut load_program_metrics,
        )
        .map_err(|_| (0, ProgramCacheEntryOwner::LoaderV1)),

        ProgramAccountLoadResult::ProgramOfLoaderV2(program_account) => ProgramCacheEntry::load(
            program_account.owner(),
            ProgramRuntimeEnvironment::clone(program_runtime_environment),
            0,
            program_account.data(),
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
                ProgramCacheEntry::load(
                    program_account.owner(),
                    ProgramRuntimeEnvironment::clone(program_runtime_environment),
                    deployment_slot,
                    programdata,
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
                    ProgramCacheEntry::load(
                        &loader_v4::id(),
                        ProgramRuntimeEnvironment::clone(program_runtime_environment),
                        deployment_slot,
                        elf_bytes,
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
        ProgramCacheEntry::new_failed_verification_tombstone(deployment_slot, owner, env)
    });

    #[cfg(feature = "metrics")]
    load_program_metrics.submit_datapoint(&mut execute_timings.details);
    loaded_program.update_access_slot(current_slot);
    Some(Arc::new(loaded_program))
}

/// Find the slot in which the program was most recently re-/deployed.
/// Returns slot 0 for programs deployed with v1/v2 loaders, since programs deployed
/// with those loaders do not retain deployment slot information.
/// Returns an error if the program's account state can not be found or parsed.
fn get_program_deployment_slot<CB: TransactionProcessingCallback>(
    callbacks: &CB,
    program: &AccountSharedData,
    loader: ProgramCacheEntryOwner,
) -> TransactionResult<Slot> {
    match loader {
        ProgramCacheEntryOwner::LoaderV1 | ProgramCacheEntryOwner::LoaderV2 => {
            if program.data().is_empty() {
                Err(TransactionError::ProgramAccountNotFound)
            } else {
                Ok(0)
            }
        }
        ProgramCacheEntryOwner::LoaderV3 => {
            if let Ok(UpgradeableLoaderState::Program {
                programdata_address,
            }) = bincode::deserialize(program.data())
            {
                let (programdata, _slot) = callbacks
                    .get_account_shared_data(&programdata_address)
                    .ok_or(TransactionError::ProgramAccountNotFound)?;
                if !bpf_loader_upgradeable::check_id(programdata.owner()) {
                    return Err(TransactionError::ProgramAccountNotFound);
                }
                if let Ok(UpgradeableLoaderState::ProgramData {
                    slot,
                    upgrade_authority_address: _,
                }) = bincode::deserialize(programdata.data())
                {
                    return Ok(slot);
                }
            }
            Err(TransactionError::ProgramAccountNotFound)
        }
        ProgramCacheEntryOwner::LoaderV4 => {
            let slot = loader_v4_get_state(program.data())
                .ok()
                .and_then(|state| {
                    (!matches!(state.status, LoaderV4Status::Retracted)).then_some(state.slot)
                })
                .ok_or(TransactionError::ProgramAccountNotFound)?;
            Ok(slot)
        }
        ProgramCacheEntryOwner::NativeLoader => unreachable!(),
    }
}

/// Returns the set of program accounts for a given batch of transactions.
pub fn filter_executable_program_accounts<'a, CB: TransactionProcessingCallback>(
    callbacks: &CB,
    program_cache_for_tx_batch: &ProgramCacheForTxBatch,
    keys: impl Iterator<Item = &'a Pubkey>,
) -> Vec<ProgramToLoad<'a>> {
    let mut result = Vec::new();
    for account_key in keys {
        if let Some(cache_entry) = program_cache_for_tx_batch.find(account_key) {
            cache_entry.stats.uses.fetch_add(1, Ordering::Relaxed);
        } else if let Some((account, _last_modification_slot)) =
            callbacks.get_account_shared_data(account_key)
        {
            let loader = if loader_v4::check_id(account.owner()) {
                ProgramCacheEntryOwner::LoaderV4
            } else if bpf_loader_upgradeable::check_id(account.owner()) {
                ProgramCacheEntryOwner::LoaderV3
            } else if bpf_loader::check_id(account.owner()) {
                ProgramCacheEntryOwner::LoaderV2
            } else if bpf_loader_deprecated::check_id(account.owner()) {
                ProgramCacheEntryOwner::LoaderV1
            } else {
                continue;
            };
            let Ok(deployment_slot) = get_program_deployment_slot(callbacks, &account, loader)
            else {
                continue;
            };
            result.push(ProgramToLoad {
                program_id: account_key,
                loader,
                deployment_slot,
            });
        }
    }
    result
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
pub mod test_utils {
    use {
        super::*,
        solana_account::WritableAccount,
        std::{fs::File, io::Read},
    };

    pub fn load_test_program() -> Vec<u8> {
        let mut dir = std::env::current_dir().unwrap();
        dir.push("tests");
        dir.push("example-programs");
        dir.push("hello-solana");
        dir.push("hello_solana_program.so");
        let mut file = File::open(dir.clone()).expect("file not found");
        let metadata = std::fs::metadata(dir).expect("Unable to read metadata");
        let mut buffer = vec![0; metadata.len() as usize];
        file.read_exact(&mut buffer).expect("Buffer overflow");
        buffer
    }

    pub fn loader_v3_program_account(programdata_address: Pubkey) -> AccountSharedData {
        let mut account = AccountSharedData::default();
        account.set_owner(bpf_loader_upgradeable::id());
        account.set_data_from_slice(
            &bincode::serialize(&UpgradeableLoaderState::Program {
                programdata_address,
            })
            .unwrap(),
        );
        account
    }

    pub fn loader_v3_programdata_account(slot: Slot, elf: &[u8]) -> AccountSharedData {
        let offset = UpgradeableLoaderState::size_of_programdata_metadata();
        let mut data = vec![0u8; offset];
        bincode::serialize_into(
            &mut data[..offset],
            &UpgradeableLoaderState::ProgramData {
                slot,
                upgrade_authority_address: None,
            },
        )
        .unwrap();
        data.extend_from_slice(elf);

        let mut account = AccountSharedData::default();
        account.set_owner(bpf_loader_upgradeable::id());
        account.set_data_from_slice(&data);
        account
    }

    pub fn loader_v4_account(slot: Slot, status: LoaderV4Status, elf: &[u8]) -> AccountSharedData {
        let mut data = vec![0u8; LoaderV4State::program_data_offset()];
        data[0..8].copy_from_slice(&slot.to_le_bytes());
        data[8..40].copy_from_slice(Pubkey::new_unique().as_ref());
        data[40..48].copy_from_slice(&(status as u64).to_le_bytes());
        data.extend_from_slice(elf);

        let mut account = AccountSharedData::default();
        account.set_owner(loader_v4::id());
        account.set_data_from_slice(&data);
        account
    }
}

#[cfg(test)]
mod tests {
    use {
        super::{test_utils::*, *},
        crate::transaction_processor::TransactionBatchProcessor,
        solana_account::WritableAccount,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_message::compiled_instruction::CompiledInstruction,
        solana_program_runtime::{
            loaded_programs::{
                BlockRelation, ForkGraph, ProgramRuntimeEnvironment,
                get_mock_program_runtime_environment,
            },
            program_cache_entry::ProgramCacheEntryType,
            solana_sbpf::program::BuiltinProgram,
        },
        solana_sdk_ids::{bpf_loader, bpf_loader_upgradeable, native_loader},
        solana_svm_transaction::svm_message::SVMMessage,
        solana_svm_type_overrides::sync::atomic::AtomicU64,
        solana_transaction::{Transaction, sanitized::SanitizedTransaction},
        std::{cell::RefCell, collections::HashMap},
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

    impl TransactionProcessingCallback for MockBankCallback {
        fn get_account_shared_data(&self, pubkey: &Pubkey) -> Option<(AccountSharedData, Slot)> {
            self.account_shared_data.borrow().get(pubkey).cloned()
        }
    }

    macro_rules! unwrap_as {
        ($result:expr, $variant:ident($($field:ident),*) $(,)?) => {
            let ($($field,)*) = match $result {
                Some(ProgramAccountLoadResult::$variant($($field),*)) => {
                    ($($field,)*)
                }
                other => panic!("Invalid result: {other:?}"),
            };
        };
    }

    #[test]
    fn test_load_program_accounts_unknown_owner() {
        let mock_bank = MockBankCallback::default();
        let key = Pubkey::new_unique();
        let mut account_data = AccountSharedData::default();
        account_data.set_owner(Pubkey::new_unique());
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key, (account_data, 0));
        assert!(load_program_accounts(&mock_bank, &key).is_none());

        // The native loader is not one of the four this checks for, so a
        // builtin's account is unknown here in exactly the same way.
        let mut account_data = AccountSharedData::default();
        account_data.set_owner(native_loader::id());
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key, (account_data, 0));
        assert!(load_program_accounts(&mock_bank, &key).is_none());
    }

    #[test]
    fn test_load_program_accounts_loader_v1() {
        let mock_bank = MockBankCallback::default();
        let key = Pubkey::new_unique();

        // Fail: account not found
        assert!(load_program_accounts(&mock_bank, &key).is_none());

        let mut account_data = AccountSharedData::default();
        account_data.set_owner(bpf_loader_deprecated::id());
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key, (account_data.clone(), 40));

        let result = load_program_accounts(&mock_bank, &key);
        unwrap_as!(result, ProgramOfLoaderV1(program));
        assert_eq!(program, account_data);
    }

    #[test]
    fn test_load_program_accounts_loader_v2() {
        let mock_bank = MockBankCallback::default();
        let key = Pubkey::new_unique();

        // Fail: account not found
        assert!(load_program_accounts(&mock_bank, &key).is_none());

        let mut account_data = AccountSharedData::default();
        account_data.set_owner(bpf_loader::id());
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key, (account_data.clone(), 40));

        let result = load_program_accounts(&mock_bank, &key);
        unwrap_as!(result, ProgramOfLoaderV2(program));
        assert_eq!(program, account_data);
    }

    #[test]
    fn test_load_program_accounts_loader_v3() {
        let mock_bank = MockBankCallback::default();
        let program_key = Pubkey::new_unique();
        let programdata_key = Pubkey::new_unique();

        // Fail: program account not found
        assert!(load_program_accounts(&mock_bank, &program_key).is_none());

        // Fail: program account invalid state
        // The load gives up before it ever looks the programdata account up.
        let mut invalid_program_account = AccountSharedData::default();
        invalid_program_account.set_owner(bpf_loader_upgradeable::id());
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(program_key, (invalid_program_account, 40));
        let result = load_program_accounts(&mock_bank, &program_key);
        unwrap_as!(result, InvalidAccountData(owner));
        assert_eq!(owner, ProgramCacheEntryOwner::LoaderV3);

        let program_account = loader_v3_program_account(programdata_key);
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(program_key, (program_account.clone(), 50));

        // Fail: programdata account missing
        // This is a bit of a footgun. The programdata account is missing, but
        // we get a modification slot of 0. This is okay as long as we know 0
        // means "not available" or null.
        let result = load_program_accounts(&mock_bank, &program_key);
        unwrap_as!(result, InvalidAccountData(owner));
        assert_eq!(owner, ProgramCacheEntryOwner::LoaderV3);

        // Fail: programdata wrong owner
        // We also need to be careful here as well, since the load fails but
        // we still get the *actual* last modified slot.
        let mut programdata_account = loader_v3_programdata_account(7, &[]);
        programdata_account.set_owner(Pubkey::new_unique());
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(programdata_key, (programdata_account, 60));
        let result = load_program_accounts(&mock_bank, &program_key);
        unwrap_as!(result, InvalidAccountData(owner));
        assert_eq!(owner, ProgramCacheEntryOwner::LoaderV3);

        // Fail: programdata invalid state
        // Same here, fails the load but the last modified slot is returned.
        let mut programdata_account = AccountSharedData::default();
        programdata_account.set_owner(bpf_loader_upgradeable::id());
        programdata_account.set_data_from_slice(&[0u8; 4]); // `Uninitialized`
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(programdata_key, (programdata_account, 60));
        let result = load_program_accounts(&mock_bank, &program_key);
        unwrap_as!(result, InvalidAccountData(owner));
        assert_eq!(owner, ProgramCacheEntryOwner::LoaderV3);

        // Success
        let programdata_account = loader_v3_programdata_account(7, &[]);
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(programdata_key, (programdata_account.clone(), 60));
        let result = load_program_accounts(&mock_bank, &program_key);
        unwrap_as!(
            result,
            ProgramOfLoaderV3(program, programdata, deployment_slot),
        );
        assert_eq!(program, program_account);
        assert_eq!(programdata, programdata_account);
        assert_eq!(deployment_slot, 7);
    }

    #[test]
    fn test_load_program_accounts_loader_v4() {
        let mock_bank = MockBankCallback::default();
        let key = Pubkey::new_unique();

        // Fail: account not found
        assert!(load_program_accounts(&mock_bank, &key).is_none());

        // Fail: invalid state
        let mut program_account = AccountSharedData::default();
        program_account.set_owner(loader_v4::id());
        program_account.set_data_from_slice(&[0u8; 4]);
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key, (program_account, 100));
        let result = load_program_accounts(&mock_bank, &key);
        unwrap_as!(result, InvalidAccountData(owner));
        assert_eq!(owner, ProgramCacheEntryOwner::LoaderV4);

        // Fail: "gifted" state
        // Sized correctly, all-zeroes. Since `LoaderV4Status::Retracted` holds
        // variant `0`, we catch this case.
        let mut program_account = AccountSharedData::default();
        program_account.set_owner(loader_v4::id());
        program_account.set_data_from_slice(&[0u8; LoaderV4State::program_data_offset()]);
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key, (program_account, 100));
        let result = load_program_accounts(&mock_bank, &key);
        unwrap_as!(result, InvalidAccountData(owner));
        assert_eq!(owner, ProgramCacheEntryOwner::LoaderV4);

        // Fail: status is Retracted
        mock_bank.account_shared_data.borrow_mut().insert(
            key,
            (loader_v4_account(9, LoaderV4Status::Retracted, &[]), 100),
        );
        let result = load_program_accounts(&mock_bank, &key);
        unwrap_as!(result, InvalidAccountData(owner));
        assert_eq!(owner, ProgramCacheEntryOwner::LoaderV4);

        // Success: any status but `Retracted`, and the state's slot is the
        // deployment slot
        for status in [LoaderV4Status::Deployed, LoaderV4Status::Finalized] {
            let program_account = loader_v4_account(9, status, &[]);
            mock_bank
                .account_shared_data
                .borrow_mut()
                .insert(key, (program_account.clone(), 100));
            let result = load_program_accounts(&mock_bank, &key);
            unwrap_as!(result, ProgramOfLoaderV4(program, deployment_slot));
            assert_eq!(program, program_account);
            assert_eq!(deployment_slot, 9);
        }
    }

    #[test]
    fn test_load_program_from_bytes() {
        let buffer = load_test_program();

        #[cfg(feature = "metrics")]
        let mut metrics = LoadProgramMetrics::default();
        let loader = bpf_loader_upgradeable::id();
        let slot: Slot = 2;
        let environment = ProgramRuntimeEnvironment::from(BuiltinProgram::new_mock());

        let result = ProgramCacheEntry::load(
            &loader,
            ProgramRuntimeEnvironment::clone(&environment),
            slot,
            &buffer,
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
        let program_key = Pubkey::new_unique();
        let programdata_key = Pubkey::new_unique();
        let mock_bank = MockBankCallback::default();
        let batch_processor = TransactionBatchProcessor::<TestForkGraph>::default();

        // Create a valid program account.
        let program_account = loader_v3_program_account(programdata_key);
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(program_key, (program_account.clone(), 50));

        // Create the programdata account as loader-owned, but with no state.
        let mut programdata_account = AccountSharedData::default();
        programdata_account.set_owner(bpf_loader_upgradeable::id());
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(programdata_key, (programdata_account, 60));

        // Just like we saw earlier in `test_load_program_accounts_loader_v3`,
        // the load result should be `InvalidAccountData(owner)`.
        let result = load_program_accounts(&mock_bank, &program_key);
        unwrap_as!(result, InvalidAccountData(owner));
        assert_eq!(owner, ProgramCacheEntryOwner::LoaderV3);

        // This is key, because we should now get a `Closed` tombstone.
        let entry = load_program_with_pubkey(
            &mock_bank,
            &batch_processor.program_runtime_environment_for_epoch(20),
            &program_key,
            100, // Slot 100
            &mut ExecuteTimings::default(),
        )
        .unwrap();

        // Assert the result matches a `Closed` tombstone.
        let closed_tombstone = ProgramCacheEntry::new_closed_tombstone(
            100, // Slot 100
            ProgramCacheEntryOwner::LoaderV3,
        );
        assert_eq!(entry, Arc::new(closed_tombstone));

        // Assert that it is NOT a `FailedVerification` tombstone.
        let fv_tombstone = ProgramCacheEntry::new_failed_verification_tombstone(
            100, // Slot 100
            ProgramCacheEntryOwner::LoaderV3,
            batch_processor.program_runtime_environment_for_epoch(20),
        );
        assert_ne!(entry, Arc::new(fv_tombstone));

        // Assert that it is NOT a `DelayVisibility` tombstone.
        let dv_tombstone = ProgramCacheEntry::new_delay_visibility_tombstone(
            100, // Slot 100
            ProgramCacheEntryOwner::LoaderV3,
            Arc::default(),
        );
        assert_ne!(entry, Arc::new(dv_tombstone));
    }

    #[test]
    fn test_load_program_invalid_elf_bytes() {
        let program_key = Pubkey::new_unique();
        let programdata_key = Pubkey::new_unique();
        let mock_bank = MockBankCallback::default();
        let batch_processor = TransactionBatchProcessor::<TestForkGraph>::default();

        // Create a valid program account.
        let program_account = loader_v3_program_account(programdata_key);
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(program_key, (program_account.clone(), 50));

        // Create a valid programdata account, but with invalid ELF bytes after
        // the metadata.
        let mut programdata_account = loader_v3_programdata_account(7, &[]);
        let mut data = programdata_account.data().to_vec();
        data.resize(UpgradeableLoaderState::size_of_programdata_metadata(), 0);
        data.extend_from_slice(&[0xff; 64]);
        programdata_account.set_data_from_slice(&data);
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(programdata_key, (programdata_account, 60));

        // The accounts themselves are fine, so the load result carries the
        // deployment slot the programdata account named.
        let result = load_program_accounts(&mock_bank, &program_key);
        unwrap_as!(
            result,
            ProgramOfLoaderV3(_program, _programdata, deployment_slot),
        );
        assert_eq!(deployment_slot, 7);

        // This is key, because the ELF fails to verify, so we should now get a
        // `FailedVerification` tombstone.
        let entry = load_program_with_pubkey(
            &mock_bank,
            &batch_processor.program_runtime_environment_for_epoch(20),
            &program_key,
            100, // Slot 100
            &mut ExecuteTimings::default(),
        )
        .unwrap();

        // Assert the result matches a `FailedVerification` tombstone.
        let fv_tombstone = ProgramCacheEntry::new_failed_verification_tombstone(
            7, // Deployment slot, not the current slot
            ProgramCacheEntryOwner::LoaderV3,
            batch_processor.program_runtime_environment_for_epoch(20),
        );
        assert_eq!(entry, Arc::new(fv_tombstone));

        // Assert that it is NOT a `Closed` tombstone.
        let closed_tombstone = ProgramCacheEntry::new_closed_tombstone(
            7, // Deployment slot, not the current slot
            ProgramCacheEntryOwner::LoaderV3,
        );
        assert_ne!(entry, Arc::new(closed_tombstone));

        // Assert that it is NOT a `DelayVisibility` tombstone.
        let dv_tombstone = ProgramCacheEntry::new_delay_visibility_tombstone(
            7, // Deployment slot, not the current slot
            ProgramCacheEntryOwner::LoaderV3,
            Arc::default(),
        );
        assert_ne!(entry, Arc::new(dv_tombstone));
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
        let entry = load_program_with_pubkey(
            &mock_bank,
            &batch_processor.program_runtime_environment_for_epoch(20),
            &key,
            200,
            &mut ExecuteTimings::default(),
        )
        .unwrap();
        let loaded_program = ProgramCacheEntry::new_failed_verification_tombstone(
            0,
            ProgramCacheEntryOwner::LoaderV2,
            batch_processor.program_runtime_environment_for_epoch(20),
        );
        assert_eq!(entry, Arc::new(loaded_program));

        let buffer = load_test_program();
        account_data.set_data_from_slice(&buffer);

        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key, (account_data.clone(), 0));

        let entry = load_program_with_pubkey(
            &mock_bank,
            &batch_processor.program_runtime_environment_for_epoch(20),
            &key,
            200,
            &mut ExecuteTimings::default(),
        )
        .unwrap();

        let program_runtime_environment = get_mock_program_runtime_environment();
        let expected = ProgramCacheEntry::load(
            account_data.owner(),
            ProgramRuntimeEnvironment::clone(&program_runtime_environment),
            0,
            account_data.data(),
            #[cfg(feature = "metrics")]
            &mut LoadProgramMetrics::default(),
        );

        assert_eq!(entry, Arc::new(expected.unwrap()));
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
        account_data.set_data_from_slice(&bincode::serialize(&state).unwrap());
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key1, (account_data.clone(), 0));

        let state = UpgradeableLoaderState::ProgramData {
            slot: 0,
            upgrade_authority_address: None,
        };
        let mut account_data2 = AccountSharedData::default();
        account_data2.set_data_from_slice(&bincode::serialize(&state).unwrap());
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key2, (account_data2.clone(), 0));

        // The programdata account is not owned by the loader, so this is a
        // `Closed` tombstone rather than a `FailedVerification` one.
        let entry = load_program_with_pubkey(
            &mock_bank,
            &batch_processor.program_runtime_environment_for_epoch(0),
            &key1,
            0,
            &mut ExecuteTimings::default(),
        )
        .unwrap();
        let loaded_program =
            ProgramCacheEntry::new_closed_tombstone(0, ProgramCacheEntryOwner::LoaderV3);
        assert_eq!(entry, Arc::new(loaded_program));

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
        account_data.set_data_from_slice(&header);

        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key2, (account_data.clone(), 0));

        let entry = load_program_with_pubkey(
            &mock_bank,
            &batch_processor.program_runtime_environment_for_epoch(20),
            &key1,
            200,
            &mut ExecuteTimings::default(),
        )
        .unwrap();

        let data = account_data.data().to_vec();
        account_data
            .set_data_from_slice(&data[UpgradeableLoaderState::size_of_programdata_metadata()..]);

        let program_runtime_environment = get_mock_program_runtime_environment();
        let expected = ProgramCacheEntry::load(
            account_data.owner(),
            ProgramRuntimeEnvironment::clone(&program_runtime_environment),
            0,
            account_data.data(),
            #[cfg(feature = "metrics")]
            &mut LoadProgramMetrics::default(),
        );
        assert_eq!(entry, Arc::new(expected.unwrap()));
    }

    #[test]
    fn test_load_program_program_loader_v4() {
        let key = Pubkey::new_unique();
        let mock_bank = MockBankCallback::default();
        let batch_processor = TransactionBatchProcessor::<TestForkGraph>::default();
        let environment = batch_processor.program_runtime_environment_for_epoch(20);

        // Retracted, closed tombstone.
        mock_bank.account_shared_data.borrow_mut().insert(
            key,
            (loader_v4_account(9, LoaderV4Status::Retracted, &[]), 100),
        );
        let entry = load_program_with_pubkey(
            &mock_bank,
            &environment,
            &key,
            200,
            &mut ExecuteTimings::default(),
        )
        .unwrap();
        assert!(matches!(entry.program, ProgramCacheEntryType::Closed));
        assert_eq!(entry.deployment_slot, 200);

        // No ELF, fail verification.
        mock_bank.account_shared_data.borrow_mut().insert(
            key,
            (loader_v4_account(9, LoaderV4Status::Deployed, &[]), 100),
        );
        let entry = load_program_with_pubkey(
            &mock_bank,
            &environment,
            &key,
            200,
            &mut ExecuteTimings::default(),
        )
        .unwrap();
        assert!(matches!(
            entry.program,
            ProgramCacheEntryType::FailedVerification(_)
        ));
        assert_eq!(entry.deployment_slot, 9);

        // Valid ELF, success.
        let mut account = loader_v4_account(9, LoaderV4Status::Deployed, &[]);
        let mut data = account.data().to_vec();
        data.extend_from_slice(&load_test_program());
        account.set_data_from_slice(&data);
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key, (account, 100));
        let entry = load_program_with_pubkey(
            &mock_bank,
            &environment,
            &key,
            200,
            &mut ExecuteTimings::default(),
        )
        .unwrap();
        assert!(matches!(entry.program, ProgramCacheEntryType::Loaded(_)));
        assert_eq!(entry.deployment_slot, 9);
        assert_eq!(entry.account_owner, ProgramCacheEntryOwner::LoaderV4);
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
            let result = load_program_with_pubkey(
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
    fn test_get_program_deployment_slot_loader_v1_v2() {
        let mock_bank = MockBankCallback::default();
        for loader in [
            ProgramCacheEntryOwner::LoaderV1,
            ProgramCacheEntryOwner::LoaderV2,
        ] {
            // Fail: invalid state
            let mut account = AccountSharedData::default();
            assert_eq!(
                get_program_deployment_slot(&mock_bank, &account, loader).err(),
                Some(TransactionError::ProgramAccountNotFound)
            );

            // Success
            account.set_data_from_slice(&[1u8; 4]);
            assert_eq!(
                get_program_deployment_slot(&mock_bank, &account, loader).unwrap(),
                0 // <-- slot is always zero for both loaders
            );
        }
    }

    #[test]
    fn test_get_program_deployment_slot_loader_v3() {
        let mock_bank = MockBankCallback::default();
        let programdata_key = Pubkey::new_unique();
        let program_account = loader_v3_program_account(programdata_key);

        // Fail: program account invalid state
        let invalid_program_account =
            AccountSharedData::new(100, 100, &bpf_loader_upgradeable::id());
        assert_eq!(
            get_program_deployment_slot(
                &mock_bank,
                &invalid_program_account,
                ProgramCacheEntryOwner::LoaderV3
            )
            .err(),
            Some(TransactionError::ProgramAccountNotFound)
        );

        // Fail: programdata account missing
        assert_eq!(
            get_program_deployment_slot(
                &mock_bank,
                &program_account,
                ProgramCacheEntryOwner::LoaderV3
            )
            .err(),
            Some(TransactionError::ProgramAccountNotFound)
        );

        // Fail: programdata account not owned by the loader
        let mut programdata_account = loader_v3_programdata_account(7, &[]);
        programdata_account.set_owner(Pubkey::new_unique());
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(programdata_key, (programdata_account, 60));
        assert_eq!(
            get_program_deployment_slot(
                &mock_bank,
                &program_account,
                ProgramCacheEntryOwner::LoaderV3
            )
            .err(),
            Some(TransactionError::ProgramAccountNotFound)
        );

        // Fail: programdata invalid state
        let mut programdata_account = AccountSharedData::default();
        programdata_account.set_owner(bpf_loader_upgradeable::id());
        programdata_account.set_data_from_slice(&[0u8; 4]); // `Uninitialized`
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(programdata_key, (programdata_account, 60));
        assert_eq!(
            get_program_deployment_slot(
                &mock_bank,
                &program_account,
                ProgramCacheEntryOwner::LoaderV3
            )
            .err(),
            Some(TransactionError::ProgramAccountNotFound)
        );

        // Success
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(programdata_key, (loader_v3_programdata_account(7, &[]), 60));
        assert_eq!(
            get_program_deployment_slot(
                &mock_bank,
                &program_account,
                ProgramCacheEntryOwner::LoaderV3
            )
            .unwrap(),
            7
        );
    }

    #[test]
    fn test_get_program_deployment_slot_loader_v4() {
        let mock_bank = MockBankCallback::default();

        // Fail: invalid state
        let mut program_account = AccountSharedData::default();
        program_account.set_owner(loader_v4::id());
        program_account.set_data_from_slice(&[0u8; 4]);
        assert_eq!(
            get_program_deployment_slot(
                &mock_bank,
                &program_account,
                ProgramCacheEntryOwner::LoaderV4
            )
            .err(),
            Some(TransactionError::ProgramAccountNotFound)
        );

        // Fail: "gifted" state
        // Sized correctly, all-zeroes. Since `LoaderV4Status::Retracted` holds
        // variant `0`, we do NOT currently catch this case.
        let mut program_account = AccountSharedData::default();
        program_account.set_owner(loader_v4::id());
        program_account.set_data_from_slice(&[0u8; LoaderV4State::program_data_offset()]);
        assert_eq!(
            get_program_deployment_slot(
                &mock_bank,
                &program_account,
                ProgramCacheEntryOwner::LoaderV4
            )
            .err(),
            Some(TransactionError::ProgramAccountNotFound)
        );

        // Fail: status is Retracted
        let account = loader_v4_account(9, LoaderV4Status::Retracted, &[]);
        assert_eq!(
            get_program_deployment_slot(&mock_bank, &account, ProgramCacheEntryOwner::LoaderV4)
                .err(),
            Some(TransactionError::ProgramAccountNotFound)
        );

        // Success
        for status in [LoaderV4Status::Deployed, LoaderV4Status::Finalized] {
            let account = loader_v4_account(9, status, &[]);
            assert_eq!(
                get_program_deployment_slot(&mock_bank, &account, ProgramCacheEntryOwner::LoaderV4)
                    .unwrap(),
                9
            );
        }
    }

    #[test]
    #[should_panic(expected = "internal error: entered unreachable code")]
    fn test_get_program_deployment_slot_native_loader() {
        let mock_bank = MockBankCallback::default();
        let mut account = AccountSharedData::default();
        account.set_owner(native_loader::id());
        let _ =
            get_program_deployment_slot(&mock_bank, &account, ProgramCacheEntryOwner::NativeLoader);
    }

    #[test]
    fn test_filter_executable_program_accounts() {
        let feepayer = Keypair::new();
        let loader_ids = [
            bpf_loader_deprecated::id(),
            bpf_loader::id(),
            bpf_loader_upgradeable::id(),
            native_loader::id(),
        ];
        let program_ids = [
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        ];
        let account_ids = [
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        ];

        let mut loaded_programs_for_tx_batch = ProgramCacheForTxBatch::default();
        let mock_bank = MockBankCallback::default();
        for i in 0..3 {
            loaded_programs_for_tx_batch.replenish(
                loader_ids[i],
                Arc::new(ProgramCacheEntry {
                    program: ProgramCacheEntryType::Builtin(BuiltinProgram::new_mock()),
                    account_owner: ProgramCacheEntryOwner::NativeLoader,
                    deployment_slot: 0,
                    stats: Arc::default(),
                    latest_access_slot: AtomicU64::default(),
                }),
            );
            mock_bank.account_shared_data.borrow_mut().insert(
                loader_ids[i],
                (AccountSharedData::new(1, 1, &program_ids[3]), 0),
            );
            mock_bank.account_shared_data.borrow_mut().insert(
                program_ids[i],
                (AccountSharedData::new(1, 1, &loader_ids[i]), 0),
            );
            mock_bank.account_shared_data.borrow_mut().insert(
                account_ids[i],
                (AccountSharedData::new(1, 1, &program_ids[i]), 0),
            );
        }

        let programdata_address = Pubkey::new_unique();
        let state = UpgradeableLoaderState::Program {
            programdata_address,
        };
        let mut program = AccountSharedData::new(1, 1, &loader_ids[2]);
        program.set_data_from_slice(&bincode::serialize(&state).unwrap());
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(program_ids[2], (program, 0));
        let state = UpgradeableLoaderState::ProgramData {
            slot: 0,
            upgrade_authority_address: None,
        };
        let mut programdata = AccountSharedData::new(1, 1, &loader_ids[2]);
        programdata.set_data_from_slice(&bincode::serialize(&state).unwrap());
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(programdata_address, (programdata, 0));

        let tx = Transaction::new_with_compiled_instructions(
            &[&feepayer],
            &[program_ids[1], program_ids[2], loader_ids[2]],
            Hash::new_unique(),
            vec![
                account_ids[0],
                account_ids[1],
                account_ids[2],
                account_ids[3],
            ],
            vec![
                CompiledInstruction::new(1, &(), vec![0, 1, 2, 3]),
                CompiledInstruction::new(2, &(), vec![0, 1, 2, 3]),
                CompiledInstruction::new(3, &(), vec![0, 1, 2, 3]),
            ],
        );
        let sanitized_tx = SanitizedTransaction::from_transaction_for_tests(tx);

        let missing_programs = filter_executable_program_accounts(
            &mock_bank,
            &loaded_programs_for_tx_batch,
            sanitized_tx.account_keys().iter(),
        );
        assert_eq!(
            missing_programs,
            &[
                ProgramToLoad {
                    program_id: &program_ids[1],
                    loader: ProgramCacheEntryOwner::LoaderV2,
                    deployment_slot: 0,
                },
                ProgramToLoad {
                    program_id: &program_ids[2],
                    loader: ProgramCacheEntryOwner::LoaderV3,
                    deployment_slot: 0,
                },
            ]
        );
    }

    #[test]
    fn test_filter_executable_program_accounts_cache_hit() {
        let mock_bank = MockBankCallback::default();
        let mut batch = ProgramCacheForTxBatch::new(10);
        let key = Pubkey::new_unique();

        let mut account = AccountSharedData::default();
        account.set_owner(bpf_loader::id());
        account.set_data_from_slice(&[1u8; 4]);
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key, (account, 0));

        let entry = Arc::new(ProgramCacheEntry::new_closed_tombstone(
            0,
            ProgramCacheEntryOwner::LoaderV2,
        ));
        batch.replenish(key, Arc::clone(&entry));

        // Already in the batch, so it is counted as used and not asked for
        // again. The account is never looked at.
        let keys = [key];
        let result = filter_executable_program_accounts(&mock_bank, &batch, keys.iter());
        assert!(result.is_empty());
        assert_eq!(entry.stats.uses.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_filter_executable_program_accounts_account_not_found() {
        let mock_bank = MockBankCallback::default();
        let batch = ProgramCacheForTxBatch::new(10);

        // The account does not exist, so there is no program to queue.
        let keys = [Pubkey::new_unique()];
        assert!(filter_executable_program_accounts(&mock_bank, &batch, keys.iter()).is_empty());
    }

    #[test]
    fn test_filter_executable_program_accounts_invalid_owner() {
        let mock_bank = MockBankCallback::default();
        let batch = ProgramCacheForTxBatch::new(10);

        // The account exists, but is not owned by any loader, so there is no
        // program to queue.
        let key = Pubkey::new_unique();
        let mut account = AccountSharedData::default();
        account.set_owner(Pubkey::new_unique());
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key, (account, 0));

        let keys = [key];
        assert!(filter_executable_program_accounts(&mock_bank, &batch, keys.iter()).is_empty());
    }

    #[test]
    fn test_filter_executable_program_accounts_loader_v1_v2() {
        let mock_bank = MockBankCallback::default();
        let batch = ProgramCacheForTxBatch::new(10);
        let key = Pubkey::new_unique();
        let keys = [key];

        for (owner, loader) in [
            (
                bpf_loader_deprecated::id(),
                ProgramCacheEntryOwner::LoaderV1,
            ),
            (bpf_loader::id(), ProgramCacheEntryOwner::LoaderV2),
        ] {
            // Empty data returns `ProgramAccountNotFound` for deployment slot,
            // so nothing is queued.
            let mut empty = AccountSharedData::default();
            empty.set_owner(owner);
            mock_bank
                .account_shared_data
                .borrow_mut()
                .insert(key, (empty, 40));
            assert!(filter_executable_program_accounts(&mock_bank, &batch, keys.iter()).is_empty());

            // Any non-empty data is considered to *maybe* be a program, and
            // both loaders always use deployment slot 0. The modification slot
            // is the program account's own.
            let mut account = AccountSharedData::default();
            account.set_owner(owner);
            account.set_data_from_slice(&[1u8; 4]);
            mock_bank
                .account_shared_data
                .borrow_mut()
                .insert(key, (account, 40));

            let result = filter_executable_program_accounts(&mock_bank, &batch, keys.iter());
            assert_eq!(result.len(), 1);
            let program_to_load = result.first().unwrap();
            assert_eq!(program_to_load.loader, loader);
            assert_eq!(program_to_load.deployment_slot, 0);
        }
    }

    #[test]
    fn test_filter_executable_program_accounts_loader_v3() {
        let mock_bank = MockBankCallback::default();
        let batch = ProgramCacheForTxBatch::new(10);
        let key = Pubkey::new_unique();
        let programdata_key = Pubkey::new_unique();
        let keys = [key];
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key, (loader_v3_program_account(programdata_key), 50));

        // Case: programdata account does not exist.
        // Can't read deployment slot, nothing is queued.
        assert!(filter_executable_program_accounts(&mock_bank, &batch, keys.iter()).is_empty());

        // Case: programdata account exists, but wrong owner.
        // Can't read deployment slot, nothing is queued.
        let mut programdata_account = loader_v3_programdata_account(7, &[]);
        programdata_account.set_owner(Pubkey::new_unique());
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(programdata_key, (programdata_account, 60));
        assert!(filter_executable_program_accounts(&mock_bank, &batch, keys.iter()).is_empty());

        // Case: programdata account exists, but wrong state.
        // Can't read deployment slot, nothing is queued.
        let mut programdata_account = AccountSharedData::default();
        programdata_account.set_owner(bpf_loader_upgradeable::id());
        programdata_account.set_data_from_slice(&[0u8; 4]); // `Uninitialized`
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(programdata_key, (programdata_account, 60));
        assert!(filter_executable_program_accounts(&mock_bank, &batch, keys.iter()).is_empty());

        // Successfully queued.
        // Both fields come from the programdata account: the deployment slot
        // *and* the modification slot.
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(programdata_key, (loader_v3_programdata_account(7, &[]), 60));
        let result = filter_executable_program_accounts(&mock_bank, &batch, keys.iter());
        assert_eq!(result.len(), 1);
        let program_to_load = result.first().unwrap();
        assert_eq!(program_to_load.loader, ProgramCacheEntryOwner::LoaderV3);
        assert_eq!(program_to_load.deployment_slot, 7);
    }

    #[test]
    fn test_filter_executable_program_accounts_loader_v4() {
        let mock_bank = MockBankCallback::default();
        let batch = ProgramCacheForTxBatch::new(10);
        let key = Pubkey::new_unique();
        let keys = [key];

        // Case: program account exists, but wrong state.
        // Can't read deployment slot, nothing is queued.
        let mut too_small = AccountSharedData::default();
        too_small.set_owner(loader_v4::id());
        too_small.set_data_from_slice(&[0u8; 4]);
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key, (too_small, 100));
        assert!(filter_executable_program_accounts(&mock_bank, &batch, keys.iter()).is_empty());

        // Case: program account is sized correctly, but all-zeroes. Since
        // `LoaderV4Status::Retracted` holds variant `0`, this is a retracted
        // program, and nothing should be queued.
        let mut gifted = AccountSharedData::default();
        gifted.set_owner(loader_v4::id());
        gifted.set_data_from_slice(&[0u8; LoaderV4State::program_data_offset()]);
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key, (gifted, 100));
        assert!(filter_executable_program_accounts(&mock_bank, &batch, keys.iter()).is_empty());

        // Case: program account holds valid state, but is `Retracted`.
        // `load_program_accounts` calls this invalid, so nothing should be
        // queued here either.
        mock_bank.account_shared_data.borrow_mut().insert(
            key,
            (loader_v4_account(9, LoaderV4Status::Retracted, &[]), 100),
        );
        assert!(filter_executable_program_accounts(&mock_bank, &batch, keys.iter()).is_empty());

        // Successfully queued.
        // Both fields come from the program account: the deployment slot from
        // its state, the modification slot from the account itself.
        mock_bank.account_shared_data.borrow_mut().insert(
            key,
            (loader_v4_account(9, LoaderV4Status::Deployed, &[]), 100),
        );
        let result = filter_executable_program_accounts(&mock_bank, &batch, keys.iter());
        assert_eq!(result.len(), 1);
        let program_to_load = result.first().unwrap();
        assert_eq!(program_to_load.loader, ProgramCacheEntryOwner::LoaderV4);
        assert_eq!(program_to_load.deployment_slot, 9);
    }

    #[test]
    fn test_loader_v4_get_state() {
        // Anything shorter than the state itself is rejected.
        for len in [0, LoaderV4State::program_data_offset().saturating_sub(1)] {
            assert!(matches!(
                loader_v4_get_state(&vec![0u8; len]),
                Err(InstructionError::AccountDataTooSmall)
            ));
        }

        // Exactly the state, and the state with program data after it, both
        // read back the fields which were written.
        for extra_bytes in [0, 8] {
            let mut account = loader_v4_account(42, LoaderV4Status::Deployed, &[]);
            let mut data = account.data().to_vec();
            data.resize(data.len().saturating_add(extra_bytes), 0);
            account.set_data_from_slice(&data);

            let state = loader_v4_get_state(account.data()).unwrap();
            assert_eq!(state.slot, 42);
            assert!(matches!(state.status, LoaderV4Status::Deployed));
        }
    }
}
