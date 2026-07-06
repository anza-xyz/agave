//! Program helpers: builtins, keyed accounts, and program cache.

#[cfg(feature = "metrics")]
use solana_program_runtime::program_metrics::LoadProgramMetrics;
use {
    crate::program_loader::load_program_with_pubkey,
    solana_account::{Account, AccountSharedData},
    solana_compute_budget::compute_budget::ComputeBudget,
    solana_program_runtime::{
        invoke_context::BuiltinFunctionRegisterer,
        loaded_programs::{ProgramCacheForTxBatch, ProgramRuntimeEnvironment},
        program_cache_entry::ProgramCacheEntry,
        solana_sbpf::program::BuiltinFunctionDefinition,
    },
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_sdk_ids::{bpf_loader, bpf_loader_deprecated, bpf_loader_upgradeable},
    solana_svm_callback::TransactionProcessingCallback,
    solana_svm_feature_set::SVMFeatureSet,
    solana_svm_timings::ExecuteTimings,
    solana_syscalls::create_program_runtime_environment,
    std::{collections::HashSet, sync::Arc},
};

struct SvmBuiltinPrototype {
    pub program_id: Pubkey,
    pub name: &'static str,
    pub register_fn: BuiltinFunctionRegisterer,
}

static SVM_BUILTINS: &[SvmBuiltinPrototype] = &[
    SvmBuiltinPrototype {
        program_id: solana_system_program::id(),
        name: "system_program",
        register_fn: solana_system_program::system_processor::Entrypoint::register,
    },
    SvmBuiltinPrototype {
        program_id: bpf_loader_deprecated::id(),
        name: "solana_bpf_loader_deprecated_program",
        register_fn: solana_bpf_loader_program::Entrypoint::register,
    },
    SvmBuiltinPrototype {
        program_id: bpf_loader::id(),
        name: "solana_bpf_loader_program",
        register_fn: solana_bpf_loader_program::Entrypoint::register,
    },
    SvmBuiltinPrototype {
        program_id: bpf_loader_upgradeable::id(),
        name: "solana_bpf_loader_upgradeable_program",
        register_fn: solana_bpf_loader_program::Entrypoint::register,
    },
    SvmBuiltinPrototype {
        program_id: solana_sdk_ids::compute_budget::id(),
        name: "compute_budget_program",
        register_fn: solana_compute_budget_program::Entrypoint::register,
    },
    #[cfg(feature = "conformance")]
    SvmBuiltinPrototype {
        program_id: solana_vote_program::id(),
        name: "vote_program",
        register_fn: solana_vote_program::vote_processor::Entrypoint::register,
    },
    #[cfg(feature = "conformance")]
    SvmBuiltinPrototype {
        program_id: solana_sdk_ids::zk_elgamal_proof_program::id(),
        name: "zk_elgamal_proof_program",
        register_fn: solana_zk_elgamal_proof_program::Entrypoint::register,
    },
];

fn create_keyed_account_for_builtin_program(program_id: &Pubkey, name: &str) -> (Pubkey, Account) {
    let data = name.as_bytes().to_vec();
    let lamports = Rent::default().minimum_balance(data.len());
    let account = Account {
        lamports,
        data,
        owner: solana_sdk_ids::native_loader::id(),
        executable: true,
        ..Default::default()
    };
    (*program_id, account)
}

pub fn keyed_account_for_system_program() -> (Pubkey, Account) {
    create_keyed_account_for_builtin_program(&SVM_BUILTINS[0].program_id, SVM_BUILTINS[0].name)
}

pub fn keyed_account_for_bpf_loader_program() -> (Pubkey, Account) {
    create_keyed_account_for_builtin_program(&SVM_BUILTINS[2].program_id, SVM_BUILTINS[2].name)
}

pub fn keyed_account_for_bpf_loader_upgradeable_program() -> (Pubkey, Account) {
    create_keyed_account_for_builtin_program(&SVM_BUILTINS[3].program_id, SVM_BUILTINS[3].name)
}

pub fn keyed_account_for_compute_budget_program() -> (Pubkey, Account) {
    create_keyed_account_for_builtin_program(&SVM_BUILTINS[4].program_id, SVM_BUILTINS[4].name)
}

pub fn new_program_cache_with_builtins(slot: u64) -> ProgramCacheForTxBatch {
    let mut cache = ProgramCacheForTxBatch::default();
    cache.set_slot_for_tests(slot);

    for builtin in SVM_BUILTINS {
        cache.replenish(
            builtin.program_id,
            Arc::new(ProgramCacheEntry::new_builtin(
                0u64,
                builtin.name.len(),
                builtin.register_fn,
            )),
        );
    }

    cache
}

/// Add a program loaded from ELF bytes to the cache.
pub fn add_program_to_program_cache(
    cache: &mut ProgramCacheForTxBatch,
    program_id: &Pubkey,
    loader_key: &Pubkey,
    elf: &[u8],
    feature_set: &SVMFeatureSet,
) {
    let compute_budget = ComputeBudget::new_with_defaults(feature_set.raise_cpi_nesting_limit_to_8);
    let program_runtime_environment = create_program_runtime_environment(
        feature_set,
        &compute_budget.to_budget(),
        false, /* reject_deployment_of_broken_elfs */
        false, /* debugging_features */
    )
    .unwrap();

    let entry = ProgramCacheEntry::new(
        loader_key,
        program_runtime_environment,
        0, // deployment_slot
        0, // effective_slot
        elf,
        elf.len(),
        #[cfg(feature = "metrics")]
        &mut LoadProgramMetrics::default(),
    )
    .unwrap();

    cache.replenish(*program_id, Arc::new(entry));
}

/// Populate a `ProgramCacheForTxBatch` via `load_program_with_pubkey` from any program accounts.
pub fn fill_program_cache_from_accounts(
    program_cache: &mut ProgramCacheForTxBatch,
    program_runtime_environment: &ProgramRuntimeEnvironment,
    accounts: &[(Pubkey, Account)],
    slot: u64,
) {
    let mut newly_loaded_programs = HashSet::<Pubkey>::new();

    for acc in accounts {
        assert!(
            newly_loaded_programs.insert(acc.0),
            "invariant violation: duplicate account load",
        );

        if program_cache.find(&acc.0).is_none() {
            if !solana_sdk_ids::bpf_loader_deprecated::check_id(&acc.1.owner)
                && !solana_sdk_ids::bpf_loader::check_id(&acc.1.owner)
                && !solana_sdk_ids::bpf_loader_upgradeable::check_id(&acc.1.owner)
            {
                continue;
            }
            if let Some((loaded_program, _last_modification_slot)) = load_program_with_pubkey(
                &FillFromAccountsCallback(accounts),
                program_runtime_environment,
                &acc.0,
                slot,
                &mut ExecuteTimings::default(),
            ) {
                program_cache.replenish(acc.0, loaded_program);
            }
        }
    }
}

struct FillFromAccountsCallback<'a>(&'a [(Pubkey, Account)]);

impl TransactionProcessingCallback for FillFromAccountsCallback<'_> {
    fn get_account_shared_data(&self, pubkey: &Pubkey) -> Option<(AccountSharedData, u64)> {
        self.0
            .iter()
            .find(|(found_pubkey, _)| *found_pubkey == *pubkey)
            .map(|(_, account)| (AccountSharedData::from(account.clone()), 0u64))
    }
}
