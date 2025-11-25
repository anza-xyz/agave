use {
    agave_feature_set::{
        enable_loader_v4, zk_elgamal_proof_program_enabled, zk_token_sdk_enabled, FeatureSet,
    },
    agave_syscalls::create_program_runtime_environment_v1,
    solana_account::{Account, AccountSharedData},
    solana_builtins::BUILTINS,
    solana_compute_budget::compute_budget::ComputeBudget,
    solana_instruction_error::InstructionError,
    solana_program_runtime::loaded_programs::{
        LoadProgramMetrics, ProgramCacheEntry, ProgramCacheForTxBatch, ProgramRuntimeEnvironments,
    },
    solana_pubkey::Pubkey,
    solana_svm_callback::{InvokeContextCallback, TransactionProcessingCallback},
    solana_svm_timings::ExecuteTimings,
    std::{collections::HashSet, sync::Arc},
};

/// Create a new `ProgramCacheForTxBatch` instance with all builtins from `solana-builtins`.
pub fn new_with_builtins(feature_set: &FeatureSet, slot: u64) -> ProgramCacheForTxBatch {
    let mut cache = ProgramCacheForTxBatch::default();
    cache.set_slot_for_tests(slot);

    for builtin in BUILTINS {
        // Only activate feature-gated builtins if the feature is active.
        if builtin.program_id == solana_sdk_ids::loader_v4::id()
            && !feature_set.is_active(&enable_loader_v4::id())
        {
            continue;
        }
        if builtin.program_id == solana_sdk_ids::zk_elgamal_proof_program::id()
            && !feature_set.is_active(&zk_elgamal_proof_program_enabled::id())
        {
            continue;
        }
        if builtin.program_id == solana_sdk_ids::zk_token_proof_program::id()
            && !feature_set.is_active(&zk_token_sdk_enabled::id())
        {
            continue;
        }

        cache.replenish(
            builtin.program_id,
            Arc::new(ProgramCacheEntry::new_builtin(
                0u64,
                builtin.name.len(),
                builtin.entrypoint,
            )),
        );
    }

    cache
}

/// Add a program loaded from ELF bytes to the cache.
pub fn add_program(
    cache: &mut ProgramCacheForTxBatch,
    program_id: &Pubkey,
    loader_key: &Pubkey,
    elf: &[u8],
    feature_set: &FeatureSet,
    compute_budget: &ComputeBudget,
) {
    let program_runtime_environment = Arc::new(
        create_program_runtime_environment_v1(
            &feature_set.runtime_features(),
            &compute_budget.to_budget(),
            false, /* reject_deployment_of_broken_elfs */
            false, /* debugging_features */
        )
        .unwrap(),
    );

    let entry = ProgramCacheEntry::new(
        loader_key,
        program_runtime_environment,
        0, // deployment_slot
        0, // effective_slot
        elf,
        elf.len(),
        &mut LoadProgramMetrics::default(),
    )
    .unwrap();

    cache.replenish(*program_id, Arc::new(entry));
}

/// Populate a `ProgramCacheForTxBatch` via `load_program_with_pubkey` from any program accounts.
pub fn fill_from_accounts(
    program_cache: &mut ProgramCacheForTxBatch,
    environments: &ProgramRuntimeEnvironments,
    accounts: &[(Pubkey, Account)],
    slot: u64,
) -> Result<(), InstructionError> {
    let mut newly_loaded_programs = HashSet::<Pubkey>::new();

    for acc in accounts {
        // FD rejects duplicate account loads
        if !newly_loaded_programs.insert(acc.0) {
            return Err(InstructionError::UnsupportedProgramId);
        }

        if program_cache.find(&acc.0).is_none() {
            // load_program_with_pubkey expects the owner to be one of the bpf loader
            if !solana_sdk_ids::loader_v4::check_id(&acc.1.owner)
                && !solana_sdk_ids::bpf_loader_deprecated::check_id(&acc.1.owner)
                && !solana_sdk_ids::bpf_loader::check_id(&acc.1.owner)
                && !solana_sdk_ids::bpf_loader_upgradeable::check_id(&acc.1.owner)
            {
                continue;
            }
            // https://github.com/anza-xyz/agave/blob/af6930da3a99fd0409d3accd9bbe449d82725bd6/svm/src/program_loader.rs#L124
            /* pub fn load_program_with_pubkey<CB: TransactionProcessingCallback, FG: ForkGraph>(
                callbacks: &CB,
                program_cache: &ProgramCache<FG>,
                pubkey: &Pubkey,
                slot: Slot,
                effective_epoch: Epoch,
                epoch_schedule: &EpochSchedule,
                reload: bool,
            ) -> Option<Arc<ProgramCacheEntry>> { */
            if let Some(loaded_program) = solana_svm::program_loader::load_program_with_pubkey(
                &FillFromAccountsCallback(accounts),
                environments,
                &acc.0,
                slot,
                &mut ExecuteTimings::default(),
                false,
            ) {
                program_cache.replenish(acc.0, loaded_program);
            }
        }
    }

    Ok(())
}

struct FillFromAccountsCallback<'a>(&'a [(Pubkey, Account)]);

impl InvokeContextCallback for FillFromAccountsCallback<'_> {}

impl TransactionProcessingCallback for FillFromAccountsCallback<'_> {
    fn get_account_shared_data(&self, pubkey: &Pubkey) -> Option<(AccountSharedData, u64)> {
        self.0
            .iter()
            .find(|(found_pubkey, _)| *found_pubkey == *pubkey)
            .map(|(_, account)| (AccountSharedData::from(account.clone()), 0u64))
    }
}
