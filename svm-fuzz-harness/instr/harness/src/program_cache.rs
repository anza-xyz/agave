use {
    solana_bpf_loader_program::syscalls::create_program_runtime_environment_v1,
    solana_builtins::BUILTINS,
    solana_compute_budget::compute_budget::ComputeBudget,
    solana_feature_set::FeatureSet,
    solana_program_runtime::loaded_programs::{
        ProgramCacheEntry, ProgramCacheForTxBatch, ProgramRuntimeEnvironments,
    },
    std::sync::Arc,
};

pub fn setup_program_cache(
    feature_set: &FeatureSet,
    compute_budget: &ComputeBudget,
    slot: u64,
) -> ProgramCacheForTxBatch {
    let mut cache = ProgramCacheForTxBatch::default();

    let environments = ProgramRuntimeEnvironments {
        program_runtime_v1: Arc::new(
            create_program_runtime_environment_v1(
                feature_set,
                compute_budget,
                false, /* deployment */
                false, /* debugging_features */
            )
            .unwrap(),
        ),
        ..ProgramRuntimeEnvironments::default()
    };

    cache.environments = environments.clone();
    cache.upcoming_environments = Some(environments);
    cache.set_slot_for_tests(slot);

    for builtin in BUILTINS {
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
