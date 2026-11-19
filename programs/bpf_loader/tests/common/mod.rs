#![allow(dead_code)]

use {
    solana_account::{Account, state_traits::StateMutWincode as _},
    solana_instruction::{AccountMeta, Instruction},
    solana_instruction_error::InstructionError,
    solana_pubkey::Pubkey,
    solana_sdk_ids::sysvar,
    solana_svm::conformance::{
        callback::ConformanceCallback,
        instr::{context::InstrContext, harness::execute_instr_with_callback},
        programs::keyed_account_for_builtin_pubkey,
    },
    solana_svm_feature_set::SVMFeatureSet,
    solana_sysvar_id::SysvarId,
};

#[derive(Clone, Copy)]
pub struct LoaderV3Features {
    /// SIMD-0433
    pub set_programdata_to_elf_length: bool,
    /// SIMD-0648
    pub unbound_loader_v3_instruction_data: bool,
    /// SIMD-0684
    pub allow_prefunded_programdata: bool,
    /// SIMD-0685
    pub remove_extend_program: bool,
}

impl LoaderV3Features {
    pub fn all_enabled() -> Self {
        Self {
            set_programdata_to_elf_length: true,
            unbound_loader_v3_instruction_data: true,
            allow_prefunded_programdata: true,
            remove_extend_program: true,
        }
    }
}

fn setup_features(feature_set: &mut SVMFeatureSet, loader_v3_features: LoaderV3Features) {
    let LoaderV3Features {
        set_programdata_to_elf_length,
        unbound_loader_v3_instruction_data,
        allow_prefunded_programdata,
        remove_extend_program,
    } = loader_v3_features;
    feature_set.loader_v3_set_program_data_to_elf_length = set_programdata_to_elf_length;
    feature_set.unbound_loader_v3_instruction_data = unbound_loader_v3_instruction_data;
    feature_set.loader_v3_allow_prefunded_programdata = allow_prefunded_programdata;
    feature_set.loader_v3_remove_extend_program = remove_extend_program;
}

pub fn create_sysvar_account<T>(value: &T) -> Account
where
    T: wincode::SchemaWrite<solana_account::WincodeConfig, Src = T>
        + for<'de> wincode::SchemaRead<'de, solana_account::WincodeConfig, Dst = T>
        + SysvarId,
{
    let serialized_len =
        wincode::config::serialized_size(value, solana_account::WINCODE_CONFIG).unwrap() as usize;
    let canonical_data_len = match T::id() {
        sysvar::clock::ID => solana_clock::SIZE,
        sysvar::epoch_schedule::ID => solana_epoch_schedule::SIZE,
        sysvar::rent::ID => solana_rent::SIZE,
        id => panic!("unsupported sysvar: {id}"),
    };
    let required_data_len = canonical_data_len.max(serialized_len);
    Account::new_data_with_space(1, value, required_data_len, &sysvar::id()).unwrap()
}

// 10 iterations is intentionally low: the harness runs on a single thread, so
// additional `shuttle` iterations validate only the harness wiring, not
// concurrent interleavings.
#[cfg(feature = "shuttle-test")]
const HARNESS_RANDOM_ITERATIONS: usize = 10;

// The conformance harness overflows Shuttle's default task stack.
#[cfg(feature = "shuttle-test")]
const CONFORMANCE_HARNESS_SHUTTLE_STACK_SIZE: usize = 1024 * 1024;

/// Run an instruction through the conformance harness, which emits a fixture
/// when `EMIT_CONFORMANCE_FIXTURES` is set.
///
/// `transaction_accounts` must include the loader's program account, as well as
/// the Clock and Rent sysvars.
pub fn process_instruction_with_conformance_harness(
    instruction: Instruction,
    transaction_accounts: Vec<(Pubkey, Account)>,
    loader_v3_features: LoaderV3Features,
    expected_result: Result<(), InstructionError>,
) -> Vec<(Pubkey, Account)> {
    let mut feature_set = SVMFeatureSet::all_enabled();
    setup_features(&mut feature_set, loader_v3_features);

    let context =
        InstrContext::new_with_default_budget(feature_set, transaction_accounts, instruction);
    execute_context_with_conformance_harness(context, expected_result)
}

/// Run an `InstrContext` through the conformance harness, which emits a fixture
/// when `EMIT_CONFORMANCE_FIXTURES` is set.
pub fn execute_context_with_conformance_harness(
    context: InstrContext,
    expected_result: Result<(), InstructionError>,
) -> Vec<(Pubkey, Account)> {
    #[cfg(feature = "shuttle-test")]
    let effects = {
        let result = shuttle::sync::Arc::new(shuttle::sync::Mutex::new(None));
        let result_for_test = shuttle::sync::Arc::clone(&result);
        let context = std::sync::Arc::new(context);
        let mut config = shuttle::Config::new();
        config.stack_size = CONFORMANCE_HARNESS_SHUTTLE_STACK_SIZE;
        shuttle::Runner::new(
            shuttle::scheduler::RandomScheduler::new(HARNESS_RANDOM_ITERATIONS),
            config,
        )
        .run(move || {
            let effects = execute_instr_with_callback(&context, &ConformanceCallback::default());
            *result_for_test.lock().unwrap() = Some(effects);
        });

        // Consume the harness cell after Shuttle exits so extraction does
        // not call `shuttle::sync::Mutex::lock` outside the scheduler.
        let Ok(mut result) = shuttle::sync::Arc::try_unwrap(result) else {
            panic!("shuttle test result still has outstanding references")
        };
        result
            .get_mut()
            .unwrap()
            .take()
            .expect("shuttle test did not produce a result")
    };

    #[cfg(not(feature = "shuttle-test"))]
    let effects = execute_instr_with_callback(&context, &ConformanceCallback::default());

    assert_eq!(effects.result, expected_result.err());

    effects.resulting_accounts
}

/// Add any accounts the conformance harness requires that the test didn't
/// provide: the Clock and Rent sysvars, and the invoked builtin's program
/// account. They go at the end, so the test's account indices still line up.
pub fn add_required_accounts(accounts: &mut Vec<(Pubkey, Account)>, program_id: &Pubkey) {
    let required = [
        Some((
            sysvar::clock::id(),
            create_sysvar_account(&solana_clock::Clock {
                slot: 1,
                ..solana_clock::Clock::default()
            }),
        )),
        Some((
            sysvar::rent::id(),
            create_sysvar_account(&solana_rent::Rent::default()),
        )),
        keyed_account_for_builtin_pubkey(program_id),
    ];
    for (address, account) in required.into_iter().flatten() {
        if !accounts.iter().any(|(key, _)| *key == address) {
            accounts.push((address, account));
        }
    }
}

/// Run an instruction through the conformance harness with all features enabled,
/// adding any accounts it requires.
pub fn process_instruction(
    program_id: &Pubkey,
    instruction_data: &[u8],
    transaction_accounts: Vec<(Pubkey, Account)>,
    instruction_accounts: Vec<AccountMeta>,
    expected_result: Result<(), InstructionError>,
) -> Vec<(Pubkey, Account)> {
    process_instruction_with_features(
        program_id,
        instruction_data,
        transaction_accounts,
        instruction_accounts,
        LoaderV3Features::all_enabled(),
        expected_result,
    )
}

/// Run an instruction through the conformance harness with all features enabled
/// and a limited compute budget, adding any accounts it requires.
pub fn process_instruction_with_cu_avail(
    program_id: &Pubkey,
    instruction_data: &[u8],
    mut transaction_accounts: Vec<(Pubkey, Account)>,
    instruction_accounts: Vec<AccountMeta>,
    cu_avail: u64,
    expected_result: Result<(), InstructionError>,
) -> Vec<(Pubkey, Account)> {
    add_required_accounts(&mut transaction_accounts, program_id);
    let mut feature_set = SVMFeatureSet::all_enabled();
    setup_features(&mut feature_set, LoaderV3Features::all_enabled());
    execute_context_with_conformance_harness(
        InstrContext {
            feature_set,
            accounts: transaction_accounts,
            instruction: Instruction::new_with_bytes(
                *program_id,
                instruction_data,
                instruction_accounts,
            ),
            cu_avail,
        },
        expected_result,
    )
}

/// Run an instruction through the conformance harness, adding any accounts it
/// requires.
pub fn process_instruction_with_features(
    program_id: &Pubkey,
    instruction_data: &[u8],
    mut transaction_accounts: Vec<(Pubkey, Account)>,
    instruction_accounts: Vec<AccountMeta>,
    loader_v3_features: LoaderV3Features,
    expected_result: Result<(), InstructionError>,
) -> Vec<(Pubkey, Account)> {
    add_required_accounts(&mut transaction_accounts, program_id);
    process_instruction_with_conformance_harness(
        Instruction::new_with_bytes(*program_id, instruction_data, instruction_accounts),
        transaction_accounts,
        loader_v3_features,
        expected_result,
    )
}
