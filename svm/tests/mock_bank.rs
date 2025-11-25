#![allow(unused)]

#[allow(deprecated)]
use solana_sysvar::recent_blockhashes::{Entry as BlockhashesEntry, RecentBlockhashes};
use {
    agave_syscalls::{
        SyscallAbort, SyscallGetClockSysvar, SyscallGetEpochScheduleSysvar, SyscallGetRentSysvar,
        SyscallInvokeSignedRust, SyscallLog, SyscallMemcmp, SyscallMemcpy, SyscallMemmove,
        SyscallMemset, SyscallSetReturnData,
    },
    solana_account::{Account, AccountSharedData, ReadableAccount, WritableAccount},
    solana_clock::{Clock, Slot, UnixTimestamp},
    solana_epoch_schedule::EpochSchedule,
    solana_fee_structure::{FeeDetails, FeeStructure},
    solana_loader_v3_interface::{self as bpf_loader_upgradeable, state::UpgradeableLoaderState},
    solana_program_runtime::{
        execution_budget::{SVMTransactionExecutionBudget, SVMTransactionExecutionCost},
        invoke_context::InvokeContext,
        loaded_programs::{BlockRelation, ForkGraph, ProgramCacheEntry},
        solana_sbpf::{
            program::{BuiltinProgram, SBPFVersion},
            vm::Config,
        },
    },
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_sdk_ids::{bpf_loader, bpf_loader_deprecated, compute_budget, loader_v4},
    solana_svm::transaction_processor::TransactionBatchProcessor,
    solana_svm_callback::{AccountState, InvokeContextCallback, TransactionProcessingCallback},
    solana_svm_feature_set::SVMFeatureSet,
    solana_svm_transaction::svm_message::SVMMessage,
    solana_svm_type_overrides::sync::{Arc, RwLock},
    solana_sysvar_id::SysvarId,
    std::{
        cmp::Ordering,
        collections::HashMap,
        env,
        fs::{self, File},
        io::Read,
    },
};

pub const EXECUTION_SLOT: u64 = 5; // The execution slot must be greater than the deployment slot
pub const EXECUTION_EPOCH: u64 = 2; // The execution epoch must be greater than the deployment epoch
pub const WALLCLOCK_TIME: i64 = 1704067200; // Arbitrarily Jan 1, 2024

pub struct MockForkGraph {}

impl ForkGraph for MockForkGraph {
    fn relationship(&self, a: Slot, b: Slot) -> BlockRelation {
        match a.cmp(&b) {
            Ordering::Less => BlockRelation::Ancestor,
            Ordering::Equal => BlockRelation::Equal,
            Ordering::Greater => BlockRelation::Descendant,
        }
    }
}

#[derive(Default, Clone)]
pub struct MockBankCallback {
    pub feature_set: SVMFeatureSet,
    pub account_shared_data: Arc<RwLock<HashMap<Pubkey, AccountSharedData>>>,
    #[allow(clippy::type_complexity)]
    pub inspected_accounts:
        Arc<RwLock<HashMap<Pubkey, Vec<(Option<AccountSharedData>, /* is_writable */ bool)>>>>,
}

impl InvokeContextCallback for MockBankCallback {}

impl TransactionProcessingCallback for MockBankCallback {
    fn get_account_shared_data(&self, pubkey: &Pubkey) -> Option<(AccountSharedData, Slot)> {
        self.account_shared_data
            .read()
            .unwrap()
            .get(pubkey)
            .map(|account| (account.clone(), 0))
    }

    fn inspect_account(&self, address: &Pubkey, account_state: AccountState, is_writable: bool) {
        let account = match account_state {
            AccountState::Dead => None,
            AccountState::Alive(account) => Some(account.clone()),
        };
        self.inspected_accounts
            .write()
            .unwrap()
            .entry(*address)
            .or_default()
            .push((account, is_writable));
    }
}

impl MockBankCallback {
    pub fn calculate_fee_details(message: &impl SVMMessage, prioritization_fee: u64) -> FeeDetails {
        let signature_count = message
            .num_transaction_signatures()
            .saturating_add(message.num_ed25519_signatures())
            .saturating_add(message.num_secp256k1_signatures())
            .saturating_add(message.num_secp256r1_signatures());

        FeeDetails::new(
            signature_count.saturating_mul(FeeStructure::default().lamports_per_signature),
            prioritization_fee,
        )
    }

    pub fn add_builtin(
        &self,
        batch_processor: &TransactionBatchProcessor<MockForkGraph>,
        program_id: Pubkey,
        name: &str,
        builtin: ProgramCacheEntry,
    ) {
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
            .insert(program_id, account_data);

        batch_processor.add_builtin(program_id, builtin);
    }

    #[allow(unused)]
    pub fn override_feature_set(&mut self, new_set: SVMFeatureSet) {
        self.feature_set = new_set
    }

    pub fn configure_sysvars(&self) {
        // We must fill in the sysvar cache entries

        // clock contents are important because we use them for a sysvar loading test
        let clock = Clock {
            slot: EXECUTION_SLOT,
            epoch_start_timestamp: WALLCLOCK_TIME.saturating_sub(10) as UnixTimestamp,
            epoch: EXECUTION_EPOCH,
            leader_schedule_epoch: EXECUTION_EPOCH,
            unix_timestamp: WALLCLOCK_TIME as UnixTimestamp,
        };

        let mut account_data = AccountSharedData::default();
        account_data.set_data(bincode::serialize(&clock).unwrap());
        self.account_shared_data
            .write()
            .unwrap()
            .insert(Clock::id(), account_data);

        // default rent is fine
        let rent = Rent::default();

        let mut account_data = AccountSharedData::default();
        account_data.set_data(bincode::serialize(&rent).unwrap());
        self.account_shared_data
            .write()
            .unwrap()
            .insert(Rent::id(), account_data);

        // SystemInstruction::AdvanceNonceAccount asserts RecentBlockhashes is
        // non-empty but then just gets the blockhash from InvokeContext. So,
        // the sysvar doesn't need real entries
        #[allow(deprecated)]
        let recent_blockhashes = vec![BlockhashesEntry::default()];

        let mut account_data = AccountSharedData::default();
        account_data.set_data(bincode::serialize(&recent_blockhashes).unwrap());
        #[allow(deprecated)]
        self.account_shared_data
            .write()
            .unwrap()
            .insert(RecentBlockhashes::id(), account_data);

        // EpochSchedule is required for non-mocked LoaderV3 deploy
        let epoch_schedule = EpochSchedule::without_warmup();

        let mut account_data = AccountSharedData::default();
        account_data.set_data(bincode::serialize(&epoch_schedule).unwrap());
        self.account_shared_data
            .write()
            .unwrap()
            .insert(EpochSchedule::id(), account_data);
    }
}

pub fn load_program(name: String) -> Vec<u8> {
    // Loading the program file
    let mut dir = env::current_dir().unwrap();
    dir.push("tests");
    dir.push("example-programs");
    dir.push(name.as_str());
    let name = name.replace('-', "_");
    dir.push(name + "_program.so");
    let mut file = File::open(dir.clone()).expect("file not found");
    let metadata = fs::metadata(dir).expect("Unable to read metadata");
    let mut buffer = vec![0; metadata.len() as usize];
    file.read_exact(&mut buffer).expect("Buffer overflow");
    buffer
}

pub fn program_address(program_name: &str) -> Pubkey {
    Pubkey::create_with_seed(&Pubkey::default(), program_name, &Pubkey::default()).unwrap()
}

pub fn program_data_size(program_name: &str) -> usize {
    load_program(program_name.to_string()).len()
}

pub fn deploy_program(name: String, deployment_slot: Slot, mock_bank: &MockBankCallback) -> Pubkey {
    deploy_program_with_upgrade_authority(name, deployment_slot, mock_bank, None)
}

pub fn deploy_program_with_upgrade_authority(
    name: String,
    deployment_slot: Slot,
    mock_bank: &MockBankCallback,
    upgrade_authority_address: Option<Pubkey>,
) -> Pubkey {
    let rent = Rent::default();
    let program_account = program_address(&name);
    let program_data_account = bpf_loader_upgradeable::get_program_data_address(&program_account);

    let state = UpgradeableLoaderState::Program {
        programdata_address: program_data_account,
    };

    // The program account must have funds and hold the executable binary
    let mut account_data = AccountSharedData::default();
    let buffer = bincode::serialize(&state).unwrap();
    account_data.set_lamports(rent.minimum_balance(buffer.len()));
    account_data.set_owner(solana_sdk_ids::bpf_loader_upgradeable::id());
    account_data.set_executable(true);
    account_data.set_data(buffer);
    mock_bank
        .account_shared_data
        .write()
        .unwrap()
        .insert(program_account, account_data);

    let mut account_data = AccountSharedData::default();
    let state = UpgradeableLoaderState::ProgramData {
        slot: deployment_slot,
        upgrade_authority_address,
    };
    let mut header = bincode::serialize(&state).unwrap();
    let mut complement = vec![
        0;
        std::cmp::max(
            0,
            UpgradeableLoaderState::size_of_programdata_metadata().saturating_sub(header.len())
        )
    ];
    let mut buffer = load_program(name);
    header.append(&mut complement);
    header.append(&mut buffer);
    account_data.set_lamports(rent.minimum_balance(header.len()));
    account_data.set_owner(solana_sdk_ids::bpf_loader_upgradeable::id());
    account_data.set_data(header);
    mock_bank
        .account_shared_data
        .write()
        .unwrap()
        .insert(program_data_account, account_data);

    program_account
}

pub fn register_builtins(
    mock_bank: &MockBankCallback,
    batch_processor: &TransactionBatchProcessor<MockForkGraph>,
    with_loader_v4: bool,
) {
    const DEPLOYMENT_SLOT: u64 = 0;
    // We must register LoaderV3 as a loadable account, otherwise programs won't execute.
    let loader_v3_name = "solana_bpf_loader_upgradeable_program";
    mock_bank.add_builtin(
        batch_processor,
        solana_sdk_ids::bpf_loader_upgradeable::id(),
        loader_v3_name,
        ProgramCacheEntry::new_builtin(
            DEPLOYMENT_SLOT,
            loader_v3_name.len(),
            solana_bpf_loader_program::Entrypoint::vm,
        ),
    );

    // Other loaders are needed for testing program cache behavior.
    let loader_v1_name = "solana_bpf_loader_deprecated_program";
    mock_bank.add_builtin(
        batch_processor,
        bpf_loader_deprecated::id(),
        loader_v1_name,
        ProgramCacheEntry::new_builtin(
            DEPLOYMENT_SLOT,
            loader_v1_name.len(),
            solana_bpf_loader_program::Entrypoint::vm,
        ),
    );

    let loader_v2_name = "solana_bpf_loader_program";
    mock_bank.add_builtin(
        batch_processor,
        bpf_loader::id(),
        loader_v2_name,
        ProgramCacheEntry::new_builtin(
            DEPLOYMENT_SLOT,
            loader_v2_name.len(),
            solana_bpf_loader_program::Entrypoint::vm,
        ),
    );

    if with_loader_v4 {
        let loader_v4_name = "solana_loader_v4_program";
        mock_bank.add_builtin(
            batch_processor,
            loader_v4::id(),
            loader_v4_name,
            ProgramCacheEntry::new_builtin(
                DEPLOYMENT_SLOT,
                loader_v4_name.len(),
                solana_loader_v4_program::Entrypoint::vm,
            ),
        );
    }

    // In order to perform a transference of native tokens using the system instruction,
    // the system program builtin must be registered.
    let system_program_name = "system_program";
    mock_bank.add_builtin(
        batch_processor,
        solana_system_program::id(),
        system_program_name,
        ProgramCacheEntry::new_builtin(
            DEPLOYMENT_SLOT,
            system_program_name.len(),
            solana_system_program::system_processor::Entrypoint::vm,
        ),
    );

    // For testing realloc, we need the compute budget program
    let compute_budget_program_name = "compute_budget_program";
    mock_bank.add_builtin(
        batch_processor,
        compute_budget::id(),
        compute_budget_program_name,
        ProgramCacheEntry::new_builtin(
            DEPLOYMENT_SLOT,
            compute_budget_program_name.len(),
            solana_compute_budget_program::Entrypoint::vm,
        ),
    );
}

pub fn create_custom_loader<'a>() -> BuiltinProgram<InvokeContext<'a, 'a>> {
    let compute_budget = SVMTransactionExecutionBudget::default();
    let vm_config = Config {
        max_call_depth: compute_budget.max_call_depth,
        stack_frame_size: compute_budget.stack_frame_size,
        enable_address_translation: true,
        enable_stack_frame_gaps: true,
        instruction_meter_checkpoint_distance: 10000,
        enable_instruction_meter: true,
        enable_register_tracing: true,
        enable_symbol_and_section_labels: true,
        reject_broken_elfs: true,
        noop_instruction_rate: 256,
        sanitize_user_provided_values: true,
        enabled_sbpf_versions: SBPFVersion::V0..=SBPFVersion::V3,
        optimize_rodata: false,
        aligned_memory_mapping: false,
    };

    // These functions are system calls the compile contract calls during execution, so they
    // need to be registered.
    let mut loader = BuiltinProgram::new_loader(vm_config);
    loader
        .register_function("abort", SyscallAbort::vm)
        .expect("Registration failed");
    loader
        .register_function("sol_log_", SyscallLog::vm)
        .expect("Registration failed");
    loader
        .register_function("sol_memcpy_", SyscallMemcpy::vm)
        .expect("Registration failed");
    loader
        .register_function("sol_memset_", SyscallMemset::vm)
        .expect("Registration failed");
    loader
        .register_function("sol_memcmp_", SyscallMemcmp::vm)
        .expect("Registration failed");
    loader
        .register_function("sol_memmove_", SyscallMemmove::vm)
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
        .register_function("sol_get_rent_sysvar", SyscallGetRentSysvar::vm)
        .expect("Registration failed");
    loader
        .register_function(
            "sol_get_epoch_schedule_sysvar",
            SyscallGetEpochScheduleSysvar::vm,
        )
        .expect("Registration failed");
    loader
}
