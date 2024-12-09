#![cfg_attr(feature = "frozen-abi", feature(min_specialization))]
#![allow(clippy::arithmetic_side_effects)]
use {
    ahash::AHashMap,
    lazy_static::lazy_static,
    solana_feature_set::{self as feature_set, FeatureSet},
    solana_pubkey::Pubkey,
    solana_sdk_ids::{
        address_lookup_table, bpf_loader, bpf_loader_deprecated, bpf_loader_upgradeable,
        compute_budget, config, ed25519_program, loader_v4, secp256k1_program, stake,
        system_program, vote,
    },
};

/// DEVELOPER: when a builtin is migrated to sbpf, please add its corresponding
/// migration feature ID to BUILTIN_INSTRUCTION_COSTS, so the builtin's default
/// cost can be determined properly based on feature status.
/// When migration completed, eg the feature gate is enabled everywhere, please
/// remove that builtin entry from BUILTIN_INSTRUCTION_COSTS.
#[derive(Clone)]
struct BuiltinCost {
    native_cost: u64,
    core_bpf_migration_feature: Option<Pubkey>,
}

lazy_static! {
    /// Number of compute units for each built-in programs
    ///
    /// DEVELOPER WARNING: This map CANNOT be modified without causing a
    /// consensus failure because this map is used to calculate the compute
    /// limit for transactions that don't specify a compute limit themselves as
    /// of https://github.com/anza-xyz/agave/issues/2212.  It's also used to
    /// calculate the cost of a transaction which is used in replay to enforce
    /// block cost limits as of
    /// https://github.com/solana-labs/solana/issues/29595.
    static ref BUILTIN_INSTRUCTION_COSTS: AHashMap<Pubkey, BuiltinCost> = [
    (
        stake::id(),
        BuiltinCost {
            native_cost: solana_stake_program::stake_instruction::DEFAULT_COMPUTE_UNITS,
            core_bpf_migration_feature: Some(feature_set::migrate_stake_program_to_core_bpf::id()),
        },
    ),
    (
        config::id(),
        BuiltinCost {
            native_cost: solana_config_program::config_processor::DEFAULT_COMPUTE_UNITS,
            core_bpf_migration_feature: Some(feature_set::migrate_config_program_to_core_bpf::id()),
        },
    ),
    (
        vote::id(),
        BuiltinCost {
            native_cost: solana_vote_program::vote_processor::DEFAULT_COMPUTE_UNITS,
            core_bpf_migration_feature: None,
        },
    ),
    (
        system_program::id(),
        BuiltinCost {
            native_cost: solana_system_program::system_processor::DEFAULT_COMPUTE_UNITS,
            core_bpf_migration_feature: None,
        },
    ),
    (
        compute_budget::id(),
        BuiltinCost {
            native_cost: solana_compute_budget_program::DEFAULT_COMPUTE_UNITS,
            core_bpf_migration_feature: None,
        },
    ),
    (
        address_lookup_table::id(),
        BuiltinCost {
            native_cost: solana_address_lookup_table_program::processor::DEFAULT_COMPUTE_UNITS,
            core_bpf_migration_feature: Some(
                feature_set::migrate_address_lookup_table_program_to_core_bpf::id(),
            ),
        },
    ),
    (
        bpf_loader_upgradeable::id(),
        BuiltinCost {
            native_cost: solana_bpf_loader_program::UPGRADEABLE_LOADER_COMPUTE_UNITS,
            core_bpf_migration_feature: None,
        },
    ),
    (
        bpf_loader_deprecated::id(),
        BuiltinCost {
            native_cost: solana_bpf_loader_program::DEPRECATED_LOADER_COMPUTE_UNITS,
            core_bpf_migration_feature: None,
        },
    ),
    (
        bpf_loader::id(),
        BuiltinCost {
            native_cost: solana_bpf_loader_program::DEFAULT_LOADER_COMPUTE_UNITS,
            core_bpf_migration_feature: None,
        },
    ),
    (
        loader_v4::id(),
        BuiltinCost {
            native_cost: solana_loader_v4_program::DEFAULT_COMPUTE_UNITS,
            core_bpf_migration_feature: None,
        },
    ),
    // Note: These are precompile, run directly in bank during sanitizing;
    (
        secp256k1_program::id(),
        BuiltinCost {
            native_cost: 0,
            core_bpf_migration_feature: None,
        },
    ),
    (
        ed25519_program::id(),
        BuiltinCost {
            native_cost: 0,
            core_bpf_migration_feature: None,
        },
    ),
    // DO NOT ADD MORE ENTRIES TO THIS MAP
    ]
    .iter()
    .cloned()
    .collect();
}

lazy_static! {
    /// A table of 256 booleans indicates whether the first `u8` of a Pubkey exists in
    /// BUILTIN_INSTRUCTION_COSTS. If the value is true, the Pubkey might be a builtin key;
    /// if false, it cannot be a builtin key. This table allows for quick filtering of
    /// builtin program IDs without the need for hashing.
    pub static ref MAYBE_BUILTIN_KEY: [bool; 256] = {
        let mut temp_table: [bool; 256] = [false; 256];
        BUILTIN_INSTRUCTION_COSTS
            .keys()
            .for_each(|key| temp_table[key.as_ref()[0] as usize] = true);
        temp_table
    };
}

pub fn get_builtin_instruction_cost<'a>(
    program_id: &'a Pubkey,
    feature_set: &'a FeatureSet,
) -> Option<u64> {
    BUILTIN_INSTRUCTION_COSTS
        .get(program_id)
        .filter(
            // Returns true if builtin program id has no core_bpf_migration_feature or feature is not activated;
            // otherwise returns false because it's not considered as builtin
            |builtin_cost| -> bool {
                builtin_cost
                    .core_bpf_migration_feature
                    .map(|feature_id| !feature_set.is_active(&feature_id))
                    .unwrap_or(true)
            },
        )
        .map(|builtin_cost| builtin_cost.native_cost)
}

#[inline]
pub fn is_builtin_program(program_id: &Pubkey) -> bool {
    BUILTIN_INSTRUCTION_COSTS.contains_key(program_id)
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_get_builtin_instruction_cost() {
        // use native cost if no migration planned
        assert_eq!(
            Some(solana_compute_budget_program::DEFAULT_COMPUTE_UNITS),
            get_builtin_instruction_cost(&compute_budget::id(), &FeatureSet::all_enabled())
        );

        // use native cost if migration is planned but not activated
        assert_eq!(
            Some(solana_stake_program::stake_instruction::DEFAULT_COMPUTE_UNITS),
            get_builtin_instruction_cost(&solana_stake_program::id(), &FeatureSet::default())
        );

        // None if migration is planned and activated, in which case, it's no longer builtin
        assert!(get_builtin_instruction_cost(
            &solana_stake_program::id(),
            &FeatureSet::all_enabled()
        )
        .is_none());

        // None if not builtin
        assert!(
            get_builtin_instruction_cost(&Pubkey::new_unique(), &FeatureSet::default()).is_none()
        );
        assert!(
            get_builtin_instruction_cost(&Pubkey::new_unique(), &FeatureSet::all_enabled())
                .is_none()
        );
    }
}
