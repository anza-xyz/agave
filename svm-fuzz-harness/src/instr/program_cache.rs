//! Local program cache for instruction/transaction processing.

use {
    solana_bpf_loader_program::syscalls::create_program_runtime_environment_v1,
    solana_compute_budget::compute_budget::ComputeBudget,
    solana_feature_set::FeatureSet,
    solana_program_runtime::{
        invoke_context::BuiltinFunctionWithContext,
        loaded_programs::{ProgramCacheEntry, ProgramCacheForTxBatch, ProgramRuntimeEnvironments},
    },
    solana_pubkey::Pubkey,
    solana_sdk_ids::{
        address_lookup_table, bpf_loader, bpf_loader_deprecated, bpf_loader_upgradeable,
        compute_budget, config, stake, system_program, vote, zk_elgamal_proof_program,
        zk_token_proof_program,
    },
    std::{collections::HashSet, sync::Arc},
};

pub struct HarnessProgramCache {
    pub builtin_program_ids: HashSet<Pubkey>,
    pub cache: ProgramCacheForTxBatch,
}

impl HarnessProgramCache {
    pub fn new(feature_set: &FeatureSet, compute_budget: &ComputeBudget) -> Self {
        let mut builtin_program_ids: HashSet<Pubkey> = HashSet::new();
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

        let mut load = |program_id: Pubkey, entrypoint: BuiltinFunctionWithContext| {
            cache.replenish(
                program_id,
                Arc::new(ProgramCacheEntry::new_builtin(0u64, 0usize, entrypoint)),
            );
            builtin_program_ids.insert(program_id);
        };

        load(
            address_lookup_table::id(),
            solana_address_lookup_table_program::processor::Entrypoint::vm,
        );
        load(
            bpf_loader_deprecated::id(),
            solana_bpf_loader_program::Entrypoint::vm,
        );
        load(bpf_loader::id(), solana_bpf_loader_program::Entrypoint::vm);
        load(
            bpf_loader_upgradeable::id(),
            solana_bpf_loader_program::Entrypoint::vm,
        );
        load(
            compute_budget::id(),
            solana_compute_budget_program::Entrypoint::vm,
        );
        load(
            config::id(),
            solana_config_program::config_processor::Entrypoint::vm,
        );
        load(
            stake::id(),
            solana_stake_program::stake_instruction::Entrypoint::vm,
        );
        load(
            system_program::id(),
            solana_system_program::system_processor::Entrypoint::vm,
        );
        load(
            vote::id(),
            solana_vote_program::vote_processor::Entrypoint::vm,
        );
        load(
            zk_elgamal_proof_program::id(),
            solana_zk_elgamal_proof_program::Entrypoint::vm,
        );
        load(
            zk_token_proof_program::id(),
            solana_zk_token_proof_program::Entrypoint::vm,
        );

        HarnessProgramCache {
            builtin_program_ids,
            cache,
        }
    }
}
