#![allow(dead_code)]

use {
    solana_account::{AccountSharedData, ReadableAccount, WritableAccount},
    solana_bpf_loader_program::{Entrypoint, test_utils},
    solana_instruction::AccountMeta,
    solana_instruction_error::InstructionError,
    solana_program_runtime::invoke_context::{
        InvokeContext, mock_process_instruction_with_feature_set,
    },
    solana_pubkey::Pubkey,
    solana_sbpf::program::BuiltinFunctionDefinition,
    solana_sdk_ids::sysvar,
    solana_svm_feature_set::SVMFeatureSet,
    solana_sysvar_id::SysvarId,
};

#[derive(Clone, Copy)]
pub struct LoaderV3Features {
    /// SIMD-0431
    pub minimum_extend_program_size: bool,
    /// SIMD-0433
    pub set_programdata_to_elf_length: bool,
}

impl LoaderV3Features {
    pub fn all_enabled() -> Self {
        Self {
            minimum_extend_program_size: true,
            set_programdata_to_elf_length: true,
        }
    }
}

fn setup_features(feature_set: &mut SVMFeatureSet, loader_v3_features: LoaderV3Features) {
    let LoaderV3Features {
        minimum_extend_program_size,
        set_programdata_to_elf_length,
    } = loader_v3_features;
    feature_set.loader_v3_minimum_extend_program_size = minimum_extend_program_size;
    feature_set.loader_v3_set_program_data_to_elf_length = set_programdata_to_elf_length;
}

pub fn create_sysvar_account<T>(value: &T) -> AccountSharedData
where
    T: wincode::Serialize<Src = T> + SysvarId,
{
    let serialized_len = wincode::serialized_size(value).unwrap() as usize;
    let canonical_data_len = match T::id() {
        sysvar::clock::ID => solana_clock::SIZE,
        sysvar::epoch_schedule::ID => solana_epoch_schedule::SIZE,
        sysvar::rent::ID => solana_rent::SIZE,
        id => panic!("unsupported sysvar: {id}"),
    };
    let required_data_len = canonical_data_len.max(serialized_len);
    let mut account = AccountSharedData::new(1, required_data_len, &sysvar::id());
    wincode::serialize_into(account.data_as_mut_slice(), value).unwrap();
    account
}

// 10 iterations is intentionally low: `mock_process_instruction` runs on a
// single thread, so additional `shuttle::check_random` iterations validate
// only the harness wiring, not concurrent interleavings. Bump this if a
// future refactor introduces `shuttle::thread::spawn` inside
// `mock_process_instruction`.
#[cfg(feature = "shuttle-test")]
const MOCK_PROCESS_RANDOM_ITERATIONS: usize = 10;

/// Wrapper around `mock_process_instruction_with_feature_set` that runs
/// under `shuttle::check_random` when the `shuttle-test` feature is
/// enabled, providing the Shuttle scheduler context required by
/// `solana-svm-type-overrides`'s shuttle-aware atomic types. With default
/// features, this is a thin pass-through to the harness with
/// `Entrypoint::register` and an empty post-adjustment closure.
///
/// The harness itself is single-threaded: the only
/// Shuttle-backed atomic in the access path is
/// `ProgramCacheEntry::latest_access_slot` (routed to
/// `shuttle::sync::atomic::AtomicU64` by `solana_svm_type_overrides`), and
/// it is touched from one Shuttle thread. Iteration-to-iteration variance
/// under `shuttle::check_random` is solely scheduler bookkeeping noise, so
/// any iteration's captured result is equivalent. If the harness ever
/// spawns Shuttle threads internally, this last-write-wins capture must
/// be re-evaluated.
///
/// `setup` is typed as `fn(&mut InvokeContext)` (function pointer, not
/// `impl Fn`) so it satisfies Shuttle's `Fn + Send + Sync + 'static` bound
/// when captured by value into the inner closure. Callers must pass
/// non-capturing closures or `fn` items; capturing closures will produce a
/// fn-pointer coercion error at the call site.
pub fn process_instruction_with_setup(
    program_id: &Pubkey,
    instruction_data: &[u8],
    transaction_accounts: Vec<(Pubkey, AccountSharedData)>,
    instruction_accounts: Vec<AccountMeta>,
    loader_v3_features: LoaderV3Features,
    expected_result: Result<(), InstructionError>,
    setup: fn(&mut InvokeContext),
) -> Vec<AccountSharedData> {
    let mut feature_set = SVMFeatureSet::all_enabled();
    setup_features(&mut feature_set, loader_v3_features);

    #[cfg(feature = "shuttle-test")]
    {
        let program_id = *program_id;
        let instruction_data = instruction_data.to_vec();
        let result = shuttle::sync::Arc::new(shuttle::sync::Mutex::new(None));
        let result_for_test = shuttle::sync::Arc::clone(&result);
        shuttle::check_random(
            move || {
                let accounts = mock_process_instruction_with_feature_set(
                    &program_id,
                    &instruction_data,
                    transaction_accounts.clone(),
                    instruction_accounts.clone(),
                    expected_result.clone(),
                    Entrypoint::register,
                    setup,
                    |_invoke_context| {},
                    &feature_set,
                );
                *result_for_test.lock().unwrap() = Some(accounts);
            },
            MOCK_PROCESS_RANDOM_ITERATIONS,
        );

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
    }

    #[cfg(not(feature = "shuttle-test"))]
    mock_process_instruction_with_feature_set(
        program_id,
        instruction_data,
        transaction_accounts,
        instruction_accounts,
        expected_result,
        Entrypoint::register,
        setup,
        |_invoke_context| {},
        &feature_set,
    )
}

pub fn process_instruction(
    program_id: &Pubkey,
    instruction_data: &[u8],
    transaction_accounts: Vec<(Pubkey, AccountSharedData)>,
    instruction_accounts: Vec<AccountMeta>,
    expected_result: Result<(), InstructionError>,
) -> Vec<AccountSharedData> {
    process_instruction_with_setup(
        program_id,
        instruction_data,
        transaction_accounts,
        instruction_accounts,
        LoaderV3Features::all_enabled(),
        expected_result,
        |invoke_context| {
            test_utils::load_all_invoked_programs(invoke_context);
        },
    )
}

pub fn truncate_data(account: &mut AccountSharedData, len: usize) {
    let mut data = account.data().to_vec();
    data.truncate(len);
    account.set_data_from_slice(&data);
}
