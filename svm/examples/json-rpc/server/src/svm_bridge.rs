use {
    agave_feature_set::FeatureSet,
    agave_syscalls::{
        SyscallAbort, SyscallGetClockSysvar, SyscallInvokeSignedRust, SyscallLog,
        SyscallLogBpfComputeUnits, SyscallLogPubkey, SyscallLogU64, SyscallMemcpy, SyscallMemset,
        SyscallSetReturnData,
    },
    log::*,
    solana_account::{Account, AccountSharedData, ReadableAccount},
    solana_clock::{Clock, Slot, UnixTimestamp},
    solana_compute_budget::compute_budget::ComputeBudget,
    solana_message::AccountKeys,
    solana_program_runtime::{
        invoke_context::InvokeContext,
        loaded_programs::{
            BlockRelation, ForkGraph, LoadProgramMetrics, ProgramCacheEntry,
            ProgramRuntimeEnvironments,
        },
        solana_sbpf::{
            program::{BuiltinProgram, SBPFVersion},
            vm::Config,
        },
    },
    solana_pubkey::Pubkey,
    solana_svm::{
        transaction_processing_result::TransactionProcessingResult,
        transaction_processor::TransactionBatchProcessor,
    },
    solana_svm_callback::{InvokeContextCallback, TransactionProcessingCallback},
    solana_sysvar_id::SysvarId,
    solana_transaction::sanitized::SanitizedTransaction,
    std::{
        collections::HashMap,
        sync::{Arc, RwLock},
        time::{SystemTime, UNIX_EPOCH},
    },
};

mod transaction {
    pub use solana_transaction_error::TransactionResult as Result;
}

const DEPLOYMENT_SLOT: u64 = 0;
const DEPLOYMENT_EPOCH: u64 = 0;

pub struct MockForkGraph {}

impl ForkGraph for MockForkGraph {
    fn relationship(&self, a: Slot, b: Slot) -> BlockRelation {
        match a.cmp(&b) {
            std::cmp::Ordering::Less => BlockRelation::Ancestor,
            std::cmp::Ordering::Equal => BlockRelation::Equal,
            std::cmp::Ordering::Greater => BlockRelation::Descendant,
        }
    }
}

pub struct MockBankCallback {
    pub feature_set: Arc<FeatureSet>,
    pub account_shared_data: RwLock<HashMap<Pubkey, AccountSharedData>>,
}

impl InvokeContextCallback for MockBankCallback {}

impl TransactionProcessingCallback for MockBankCallback {
    fn account_matches_owners(&self, account: &Pubkey, owners: &[Pubkey]) -> Option<usize> {
        if let Some(data) = self.account_shared_data.read().unwrap().get(account) {
            if data.lamports() == 0 {
                None
            } else {
                owners.iter().position(|entry| data.owner() == entry)
            }
        } else {
            None
        }
    }

    fn get_account_shared_data(&self, pubkey: &Pubkey) -> Option<AccountSharedData> {
        debug!(
            "Get account {pubkey} shared data, thread {:?}",
            std::thread::current().name()
        );
        self.account_shared_data
            .read()
            .unwrap()
            .get(pubkey)
            .cloned()
    }

    fn add_builtin_account(&self, name: &str, program_id: &Pubkey) {
        let account_data = AccountSharedData::from(Account {
            lamports: 5000,
            data: name.as_bytes().to_vec(),
            owner: solana_sdk_ids::native_loader::id(),
            executable: true,
            rent_epoch: 0,
        });

        self.account_shared_data
            .write()
            .unwrap()
            .insert(*program_id, account_data);
    }
}

impl MockBankCallback {
    pub fn new(account_map: Vec<(Pubkey, AccountSharedData)>) -> Self {
        Self {
            feature_set: Arc::new(FeatureSet::default()),
            account_shared_data: RwLock::new(HashMap::from_iter(account_map)),
        }
    }

    #[allow(dead_code)]
    pub fn override_feature_set(&mut self, new_set: FeatureSet) {
        self.feature_set = Arc::new(new_set)
    }
}

pub struct LoadAndExecuteTransactionsOutput {
    // Vector of results indicating whether a transaction was executed or could not
    // be executed. Note executed transactions can still have failed!
    pub processing_results: Vec<TransactionProcessingResult>,
}

pub struct TransactionBatch<'a> {
    lock_results: Vec<transaction::Result<()>>,
    sanitized_txs: std::borrow::Cow<'a, [SanitizedTransaction]>,
}

impl<'a> TransactionBatch<'a> {
    pub fn new(
        lock_results: Vec<transaction::Result<()>>,
        sanitized_txs: std::borrow::Cow<'a, [SanitizedTransaction]>,
    ) -> Self {
        assert_eq!(lock_results.len(), sanitized_txs.len());
        Self {
            lock_results,
            sanitized_txs,
        }
    }

    pub fn lock_results(&self) -> &Vec<transaction::Result<()>> {
        &self.lock_results
    }

    pub fn sanitized_transactions(&self) -> &[SanitizedTransaction] {
        &self.sanitized_txs
    }
}

pub fn create_custom_environment<'a>() -> BuiltinProgram<InvokeContext<'a>> {
    let compute_budget = ComputeBudget::new_with_defaults(/* simd_0296_active */ false);
    let vm_config = Config {
        max_call_depth: compute_budget.max_call_depth,
        stack_frame_size: compute_budget.stack_frame_size,
        enable_address_translation: true,
        enable_stack_frame_gaps: true,
        instruction_meter_checkpoint_distance: 10000,
        enable_instruction_meter: true,
        enable_instruction_tracing: true,
        enable_symbol_and_section_labels: true,
        reject_broken_elfs: true,
        noop_instruction_rate: 256,
        sanitize_user_provided_values: true,
        enabled_sbpf_versions: SBPFVersion::V0..=SBPFVersion::V3,
        optimize_rodata: false,
        aligned_memory_mapping: true,
    };

    // Register system calls that the compiled contract calls during execution.
    let mut loader = BuiltinProgram::new_loader(vm_config);
    loader
        .register_function("abort", SyscallAbort::vm)
        .expect("Registration failed");
    loader
        .register_function("sol_log_", SyscallLog::vm)
        .expect("Registration failed");
    loader
        .register_function("sol_log_64_", SyscallLogU64::vm)
        .expect("Registration failed");
    loader
        .register_function("sol_log_compute_units_", SyscallLogBpfComputeUnits::vm)
        .expect("Registration failed");
    loader
        .register_function("sol_log_pubkey", SyscallLogPubkey::vm)
        .expect("Registration failed");
    loader
        .register_function("sol_memcpy_", SyscallMemcpy::vm)
        .expect("Registration failed");
    loader
        .register_function("sol_memset_", SyscallMemset::vm)
        .expect("Registration failed");
    loader
        .register_function("sol_invoke_signed_rust", SyscallInvokeSignedRust::vm)
        .expect("Registration failed");
    loader
        .register_function("sol_set_return_data", SyscallSetReturnData::vm)
        .expect("Registration failed");
    loader
        .register_function("sol_get_clock_sysvar", SyscallGetClockSysvar::vm)
        .expect("Registration failed");
    loader
}

pub fn create_executable_environment(
    fork_graph: Arc<RwLock<MockForkGraph>>,
    account_keys: &AccountKeys,
    mock_bank: &mut MockBankCallback,
    transaction_processor: &TransactionBatchProcessor<MockForkGraph>,
) {
    let mut program_cache = transaction_processor.program_cache.write().unwrap();

    program_cache.environments = ProgramRuntimeEnvironments {
        program_runtime_v1: Arc::new(create_custom_environment()),
        // We are not using program runtime v2
        program_runtime_v2: Arc::new(BuiltinProgram::new_loader(Config::default())),
    };

    program_cache.fork_graph = Some(Arc::downgrade(&fork_graph));
    // add programs to cache
    for key in account_keys.iter() {
        if let Some(account) = mock_bank.get_account_shared_data(key) {
            if account.executable()
                && *account.owner() == solana_sdk_ids::bpf_loader_upgradeable::id()
            {
                let data = account.data();
                let program_data_account_key = Pubkey::try_from(data[4..].to_vec()).unwrap();
                let program_data_account = mock_bank
                    .get_account_shared_data(&program_data_account_key)
                    .unwrap();
                let program_data = program_data_account.data();
                let elf_bytes = program_data[45..].to_vec();

                let program_runtime_environment =
                    program_cache.environments.program_runtime_v1.clone();
                program_cache.assign_program(
                    *key,
                    Arc::new(
                        ProgramCacheEntry::new(
                            &solana_sdk_ids::bpf_loader_upgradeable::id(),
                            program_runtime_environment,
                            0,
                            0,
                            &elf_bytes,
                            elf_bytes.len(),
                            &mut LoadProgramMetrics::default(),
                        )
                        .unwrap(),
                    ),
                );
            }
        }
    }

    // We must fill in the sysvar cache entries
    let time_now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("Time went backwards")
        .as_secs() as i64;
    let clock = Clock {
        slot: DEPLOYMENT_SLOT,
        epoch_start_timestamp: time_now.saturating_sub(10) as UnixTimestamp,
        epoch: DEPLOYMENT_EPOCH,
        leader_schedule_epoch: DEPLOYMENT_EPOCH,
        unix_timestamp: time_now as UnixTimestamp,
    };

    let mut account_data = AccountSharedData::default();
    account_data.set_data_from_slice(bincode::serialize(&clock).unwrap().as_slice());
    mock_bank
        .account_shared_data
        .write()
        .unwrap()
        .insert(Clock::id(), account_data);
}
