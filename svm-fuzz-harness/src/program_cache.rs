use {
    agave_feature_set::{
        enable_loader_v4, zk_elgamal_proof_program_enabled, zk_token_sdk_enabled, FeatureSet,
    },
    solana_builtins::BUILTINS,
    solana_program_runtime::loaded_programs::{ProgramCacheEntry, ProgramCacheForTxBatch},
    solana_pubkey::Pubkey,
    std::sync::Arc,
};

// These programs have been migrated to Core BPF, and therefore should not be
// included in the fuzzing harness.
const MIGRATED_BUILTINS: &[Pubkey] = &[
    solana_sdk_ids::address_lookup_table::id(),
    solana_sdk_ids::config::id(),
    solana_sdk_ids::stake::id(),
];

pub fn setup_program_cache(feature_set: &FeatureSet, slot: u64) -> ProgramCacheForTxBatch {
    let mut cache = ProgramCacheForTxBatch::default();
    cache.set_slot_for_tests(slot);

    for builtin in BUILTINS {
        // Skip migrated builtins.
        if MIGRATED_BUILTINS.contains(&builtin.program_id) {
            continue;
        }

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
