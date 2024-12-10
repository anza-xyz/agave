//! Configurations for handling cost modeling of builtin programs.

use {
    crate::BUILTINS,
    ahash::AHashMap,
    lazy_static::lazy_static,
    solana_feature_set::FeatureSet,
    solana_pubkey::Pubkey,
    solana_sdk_ids::{ed25519_program, secp256k1_program},
};

/// Configuration for cost modeling of a builtin program.
#[derive(Debug)]
pub struct CostModelingConfig {
    /// The default cost of the builtin program.
    /// If None, this builtin program is not cost modeled.
    pub default_cost: Option<u64>,
    /// Configuration for updating the cost of the builtin program.
    /// If None, the cost is never updated.
    pub new_cost_config: Option<NewCostModelingConfig>,
}

/// Configuration for updating the cost of a builtin program.
#[derive(Clone, Copy, Debug)]
pub struct NewCostModelingConfig {
    /// The new cost of the builtin program.
    pub new_cost: u64,
    /// The feature gate to trigger the new cost.
    pub feature_id: Pubkey,
}

struct BuiltinCost {
    default_cost: Option<u64>,
    core_bpf_migration_feature: Option<Pubkey>,
    new_cost_config: Option<NewCostModelingConfig>,
}

lazy_static! {
    static ref BUILTIN_INSTRUCTION_COSTS: AHashMap<Pubkey, BuiltinCost> = BUILTINS
        .iter()
        .map(|builtin| {
            (
                builtin.program_id,
                BuiltinCost {
                    default_cost: builtin.cost_modeling_config.default_cost,
                    core_bpf_migration_feature: builtin
                        .core_bpf_migration_config
                        .as_ref()
                        .map(|config| config.feature_id),
                    new_cost_config: builtin.cost_modeling_config.new_cost_config,
                },
            )
        })
        .chain(
            [
                (
                    secp256k1_program::id(),
                    BuiltinCost {
                        default_cost: Some(0),
                        core_bpf_migration_feature: None,
                        new_cost_config: None,
                    },
                ),
                (
                    ed25519_program::id(),
                    BuiltinCost {
                        default_cost: Some(0),
                        core_bpf_migration_feature: None,
                        new_cost_config: None,
                    },
                ),
            ]
            .into_iter()
        )
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
        .and_then(|builtin_cost| {
            // If the program has a Core BPF Migration feature and that feature
            // is active, then the program is not considered a builtin.
            if builtin_cost
                .core_bpf_migration_feature
                .is_some_and(|feature_id| feature_set.is_active(&feature_id))
            {
                return None;
            }
            // If the program has a new cost configuration and that feature is
            // active, then return the new cost.
            if builtin_cost
                .new_cost_config
                .is_some_and(|new_cost_config| feature_set.is_active(&new_cost_config.feature_id))
            {
                return Some(builtin_cost.new_cost_config.unwrap().new_cost);
            }
            // Otherwise, return the default cost.
            builtin_cost.default_cost
        })
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
            get_builtin_instruction_cost(
                &solana_sdk_ids::compute_budget::id(),
                &FeatureSet::all_enabled()
            )
        );

        // use native cost if migration is planned but not activated
        assert_eq!(
            Some(solana_stake_program::stake_instruction::DEFAULT_COMPUTE_UNITS),
            get_builtin_instruction_cost(&solana_sdk_ids::stake::id(), &FeatureSet::default())
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
