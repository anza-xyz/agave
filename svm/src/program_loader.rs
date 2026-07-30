#[cfg(feature = "metrics")]
use solana_program_runtime::program_metrics::LoadProgramMetrics;
use {
    solana_account::{AccountSharedData, ReadableAccount},
    solana_clock::Slot,
    solana_instruction::error::InstructionError,
    solana_loader_v3_interface::state::UpgradeableLoaderState,
    solana_loader_v4_interface::state::{LoaderV4State, LoaderV4Status},
    solana_program_runtime::{
        callback::ProgramCacheCallback,
        loaded_programs::{
            ForkGraph, ProgramCache, ProgramCacheForTxBatch, ProgramCacheMatchCriteria,
            ProgramRuntimeEnvironment, ProgramToLoad,
        },
        program_cache_entry::{ProgramCacheEntry, ProgramCacheEntryOwner, ProgramCacheEntryType},
    },
    solana_pubkey::Pubkey,
    solana_sdk_ids::{bpf_loader, bpf_loader_deprecated, bpf_loader_upgradeable, loader_v4},
    solana_svm_callback::TransactionProcessingCallback,
    solana_svm_measure::measure_us,
    solana_svm_timings::{ExecuteTimingType, ExecuteTimings},
    solana_svm_type_overrides::sync::{Arc, RwLock},
    solana_transaction_error::{TransactionError, TransactionResult},
    std::{
        cell::{Cell, RefCell},
        collections::HashSet,
        sync::atomic::Ordering,
    },
};

#[derive(Debug)]
pub(crate) enum ProgramAccountLoadResult {
    InvalidAccountData(ProgramCacheEntryOwner),
    ProgramOfLoaderV1(AccountSharedData),
    ProgramOfLoaderV2(AccountSharedData),
    ProgramOfLoaderV3(AccountSharedData, AccountSharedData, Slot),
    ProgramOfLoaderV4(AccountSharedData, Slot),
}

/// A [ProgramCacheCallback] that extracts one program at a time from the global
/// program cache, as each is invoked, instead of provisioning the batch-local
/// cache up front.
///
/// Scoped to a single transaction. Loads happen through `&self`, so their
/// side effects are staged and drained afterwards.
pub(crate) struct ProgramLoader<'a, CB: TransactionProcessingCallback, FG: ForkGraph> {
    global_program_cache: &'a RwLock<ProgramCache<FG>>,
    callbacks: &'a CB,
    slot: Slot,
    program_runtime_environment_for_execution: &'a ProgramRuntimeEnvironment,
    check_program_deployment_slot: bool,
    limit_to_load_programs: bool,
    loaded_missing: Cell<bool>,
    hit_max_limit: Cell<bool>,
    timings: RefCell<ExecuteTimings>,
    /// Programs already counted against this transaction, so that a program
    /// invoked more than once is only counted once.
    counted: RefCell<HashSet<Pubkey>>,
}

impl<'a, CB: TransactionProcessingCallback, FG: ForkGraph> ProgramLoader<'a, CB, FG> {
    pub(crate) fn new(
        global_program_cache: &'a RwLock<ProgramCache<FG>>,
        callbacks: &'a CB,
        slot: Slot,
        program_runtime_environment_for_execution: &'a ProgramRuntimeEnvironment,
        check_program_deployment_slot: bool,
        limit_to_load_programs: bool,
    ) -> Self {
        Self {
            global_program_cache,
            callbacks,
            slot,
            program_runtime_environment_for_execution,
            check_program_deployment_slot,
            limit_to_load_programs,
            loaded_missing: Cell::new(false),
            hit_max_limit: Cell::new(false),
            timings: RefCell::new(ExecuteTimings::default()),
            counted: RefCell::new(HashSet::new()),
        }
    }

    pub(crate) fn loaded_missing(&self) -> bool {
        self.loaded_missing.get()
    }

    pub(crate) fn hit_max_limit(&self) -> bool {
        self.hit_max_limit.get()
    }

    pub(crate) fn take_timings(&self) -> ExecuteTimings {
        self.timings.take()
    }
}

impl<CB: TransactionProcessingCallback, FG: ForkGraph> ProgramCacheCallback
    for ProgramLoader<'_, CB, FG>
{
    fn load_program(&self, program_id: &Pubkey) -> Option<Arc<ProgramCacheEntry>> {
        // A scratch view to extract through, since the batch-local cache is
        // already mutably borrowed at this point.
        let mut extracted = ProgramCacheForTxBatch::new(self.slot);

        let (missing_programs, filter_executable_us) =
            measure_us!(filter_executable_program_accounts(
                self.callbacks,
                &extracted,
                std::iter::once(program_id),
                self.check_program_deployment_slot,
            ));
        let mut timings = self.timings.borrow_mut();
        timings
            .saturating_add_in_place(ExecuteTimingType::FilterExecutableUs, filter_executable_us);

        // The account is absent, or is not owned by a loader.
        if missing_programs.is_empty() {
            return None;
        }

        let ((), program_cache_us) = measure_us!(replenish_program_cache(
            self.global_program_cache,
            self.callbacks,
            self.slot,
            self.program_runtime_environment_for_execution,
            missing_programs,
            &mut extracted,
            &mut timings,
            self.limit_to_load_programs,
            /* increment_usage_counter */ true,
        ));
        timings.saturating_add_in_place(ExecuteTimingType::ProgramCacheUs, program_cache_us);

        self.loaded_missing
            .set(self.loaded_missing.get() | extracted.loaded_missing);
        self.hit_max_limit
            .set(self.hit_max_limit.get() | extracted.hit_max_limit);

        // `extract` already counted this load.
        self.counted.borrow_mut().insert(*program_id);

        extracted.find(program_id)
    }

    fn record_program_use(&self, program_id: &Pubkey, entry: &Arc<ProgramCacheEntry>) {
        if self.counted.borrow_mut().insert(*program_id) {
            entry.stats.uses.fetch_add(1, Ordering::Relaxed);
        }
    }
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
            program_account.data(),
            #[cfg(feature = "metrics")]
            &mut load_program_metrics,
        )
        .map_err(|_| (0, ProgramCacheEntryOwner::LoaderV1)),

        ProgramAccountLoadResult::ProgramOfLoaderV2(program_account) => ProgramCacheEntry::new(
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
                ProgramCacheEntry::new(
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
                    ProgramCacheEntry::new(
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
    loader: ProgramCacheEntryOwner,
) -> TransactionResult<Slot> {
    match loader {
        ProgramCacheEntryOwner::LoaderV1 | ProgramCacheEntryOwner::LoaderV2 => Ok(0),
        ProgramCacheEntryOwner::LoaderV3 => {
            if let Ok(UpgradeableLoaderState::Program {
                programdata_address,
            }) = bincode::deserialize(program.data())
            {
                let (programdata, _slot) = callbacks
                    .get_account_shared_data(&programdata_address)
                    .ok_or(TransactionError::ProgramAccountNotFound)?;
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
            let state = loader_v4_get_state(program.data())
                .map_err(|_| TransactionError::ProgramAccountNotFound)?;
            Ok(state.slot)
        }
        ProgramCacheEntryOwner::NativeLoader => unreachable!(),
    }
}

/// Appends to a set of executable program accounts (all accounts owned by any loader)
/// for transactions with a valid blockhash or nonce.
pub fn filter_executable_program_accounts<'a, CB: TransactionProcessingCallback>(
    callbacks: &CB,
    program_cache_for_tx_batch: &ProgramCacheForTxBatch,
    keys: impl Iterator<Item = &'a Pubkey>,
    check_program_deployment_slot: bool,
) -> Vec<ProgramToLoad<'a>> {
    let mut result = Vec::new();
    for account_key in keys {
        if let Some(cache_entry) = program_cache_for_tx_batch.find(account_key) {
            cache_entry.stats.uses.fetch_add(1, Ordering::Relaxed);
        } else if let Some((account, last_modification_slot)) =
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
            let match_criteria = if check_program_deployment_slot {
                get_program_deployment_slot(callbacks, &account, loader)
                    .map_or(ProgramCacheMatchCriteria::Tombstone, |slot| {
                        ProgramCacheMatchCriteria::DeployedOnOrAfterSlot(slot)
                    })
            } else {
                ProgramCacheMatchCriteria::NoCriteria
            };
            result.push(ProgramToLoad {
                program_id: account_key,
                loader,
                match_criteria,
                last_modification_slot,
            });
        }
    }
    result
}

/// Cooperatively load each of `missing_programs` from the global program cache
/// into `program_cache_for_tx_batch`, compiling any that are not resident yet.
#[allow(clippy::too_many_arguments)]
pub fn replenish_program_cache<CB: TransactionProcessingCallback, FG: ForkGraph>(
    global_program_cache: &RwLock<ProgramCache<FG>>,
    callbacks: &CB,
    slot: Slot,
    program_runtime_environment_for_execution: &ProgramRuntimeEnvironment,
    mut missing_programs: Vec<ProgramToLoad>,
    program_cache_for_tx_batch: &mut ProgramCacheForTxBatch,
    execute_timings: &mut ExecuteTimings,
    limit_to_load_programs: bool,
    increment_usage_counter: bool,
) {
    if missing_programs.is_empty() {
        // Nothing to load, so skip the global cache and fork graph locks.
        // Program-cache hit/miss counters are unchanged for empty work.
        return;
    }
    let mut count_hits_and_misses = true;
    loop {
        // Lock the global cache.
        let global_cache = global_program_cache.read().unwrap();
        // Figure out which program needs to be loaded next.
        let program_to_load = global_cache.extract(
            &mut missing_programs,
            program_cache_for_tx_batch,
            program_runtime_environment_for_execution,
            increment_usage_counter,
            count_hits_and_misses,
        );
        count_hits_and_misses = false;
        let task_waiter = Arc::clone(&global_cache.loading_task_waiter);
        let task_cookie = task_waiter.cookie();
        // Unlock the global cache again.
        drop(global_cache);

        let program_to_store = program_to_load.map(|key| {
            // Load, verify and compile one program.
            let (program, last_modification_slot) = load_program_with_pubkey(
                callbacks,
                program_runtime_environment_for_execution,
                &key,
                slot,
                execute_timings,
            )
            .expect("called load_program_with_pubkey() with nonexistent account");
            (key, program, last_modification_slot)
        });

        if let Some((key, program, last_modification_slot)) = program_to_store {
            program_cache_for_tx_batch.loaded_missing = true;
            let mut global_cache = global_program_cache.write().unwrap();
            // Submit our last completed loading task.
            if global_cache.finish_cooperative_loading_task(
                program_runtime_environment_for_execution,
                slot,
                key,
                last_modification_slot,
                program,
            ) && limit_to_load_programs
            {
                // This branch is taken when there is an error in assigning a program to a
                // cache slot. It is not possible to mock this error for SVM unit
                // tests purposes.
                *program_cache_for_tx_batch = ProgramCacheForTxBatch::new(slot);
                program_cache_for_tx_batch.hit_max_limit = true;
                return;
            }
        } else if missing_programs.is_empty() {
            break;
        } else {
            // Remember: there are multiple transaction processor threads running concurrently
            // and those other threads may be loading this or other programs.
            //
            // So, sleep until some other thread submits a program with their
            // `finish_cooperative_loading_task` call. We'll then wake up and try to load the
            // missing programs inside the tx batch again.
            let _new_cookie = task_waiter.wait(task_cookie);
        }
    }
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
        solana_message::compiled_instruction::CompiledInstruction,
        solana_program_runtime::{
            loaded_programs::{
                BlockRelation, ForkGraph, ProgramRuntimeEnvironment,
                get_mock_program_runtime_environment,
            },
            solana_sbpf::program::BuiltinProgram,
        },
        solana_sdk_ids::{bpf_loader, bpf_loader_upgradeable, native_loader},
        solana_svm_transaction::svm_message::SVMMessage,
        solana_svm_type_overrides::sync::atomic::AtomicU64,
        solana_transaction::{Transaction, sanitized::SanitizedTransaction},
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
        let slot: Slot = 2;
        let environment = ProgramRuntimeEnvironment::from(BuiltinProgram::new_mock());

        let result = ProgramCacheEntry::new(
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
            account_data.data(),
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
            account_data.data(),
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
            ProgramCacheEntryOwner::LoaderV3,
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
            ProgramCacheEntryOwner::LoaderV3,
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
            .insert(key2, (account_data.clone(), 0));

        let result = get_program_deployment_slot(
            &mock_bank,
            &mock_bank.get_account_shared_data(&key1).unwrap().0,
            ProgramCacheEntryOwner::LoaderV3,
        );
        assert_eq!(result.unwrap(), 77);
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
            false,
        );
        assert_eq!(
            missing_programs,
            &[
                ProgramToLoad {
                    program_id: &program_ids[1],
                    loader: ProgramCacheEntryOwner::LoaderV2,
                    match_criteria: ProgramCacheMatchCriteria::NoCriteria,
                    last_modification_slot: 0,
                },
                ProgramToLoad {
                    program_id: &program_ids[2],
                    loader: ProgramCacheEntryOwner::LoaderV3,
                    match_criteria: ProgramCacheMatchCriteria::NoCriteria,
                    last_modification_slot: 0,
                },
            ]
        );

        let missing_programs = filter_executable_program_accounts(
            &mock_bank,
            &loaded_programs_for_tx_batch,
            sanitized_tx.account_keys().iter(),
            true,
        );
        assert_eq!(
            missing_programs,
            &[
                ProgramToLoad {
                    program_id: &program_ids[1],
                    loader: ProgramCacheEntryOwner::LoaderV2,
                    match_criteria: ProgramCacheMatchCriteria::DeployedOnOrAfterSlot(0),
                    last_modification_slot: 0,
                },
                ProgramToLoad {
                    program_id: &program_ids[2],
                    loader: ProgramCacheEntryOwner::LoaderV3,
                    match_criteria: ProgramCacheMatchCriteria::Tombstone,
                    last_modification_slot: 0,
                },
            ]
        );
    }
}
