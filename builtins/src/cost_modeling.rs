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
pub enum CostModelingConfig {
    /// The builtin program is cost modeled.
    CostModeled {
        /// The default cost of the builtin program.
        default_cost: u64,
    },
    /// The builtin program is not cost modeled.
    NotCostModeled,
}

struct BuiltinCost {
    default_cost: u64,
    core_bpf_migration_feature: Option<Pubkey>,
}

lazy_static! {
    static ref BUILTIN_INSTRUCTION_COSTS: AHashMap<Pubkey, BuiltinCost> = BUILTINS
        .iter()
        .filter_map(|builtin| {
            match builtin.cost_modeling_config {
                CostModelingConfig::CostModeled { default_cost } => Some((
                    builtin.program_id,
                    BuiltinCost {
                        default_cost,
                        core_bpf_migration_feature: builtin
                            .core_bpf_migration_config
                            .as_ref()
                            .map(|config| config.feature_id),
                    },
                )),
                CostModelingConfig::NotCostModeled => {
                    None
                }
            }
        })
        .chain(
            [
                (
                    secp256k1_program::id(),
                    BuiltinCost {
                        default_cost: 0, // Hard-coded to zero.
                        core_bpf_migration_feature: None,
                    },
                ),
                (
                    ed25519_program::id(),
                    BuiltinCost {
                        default_cost: 0, // Hard-coded to zero.
                        core_bpf_migration_feature: None,
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
            // Otherwise, return the default cost.
            Some(builtin_cost.default_cost)
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
    fn test_maybe_builtin_key() {
        let check = |key: &Pubkey, expected: bool| {
            assert_eq!(MAYBE_BUILTIN_KEY[key.as_ref()[0] as usize], expected);
            assert_eq!(is_builtin_program(key), expected);
        };

        check(&solana_sdk_ids::system_program::id(), true);
        check(&solana_sdk_ids::vote::id(), true);
        check(&solana_sdk_ids::stake::id(), true);
        check(&solana_sdk_ids::config::id(), true);
        check(&solana_sdk_ids::bpf_loader_deprecated::id(), true);
        check(&solana_sdk_ids::bpf_loader::id(), true);
        check(&solana_sdk_ids::bpf_loader_upgradeable::id(), true);
        check(&solana_sdk_ids::compute_budget::id(), true);
        check(&solana_sdk_ids::address_lookup_table::id(), true);
        check(&solana_sdk_ids::loader_v4::id(), true);
        check(&solana_sdk_ids::ed25519_program::id(), true);
        check(&solana_sdk_ids::secp256k1_program::id(), true);

        // Not cost modeled.
        check(&solana_sdk_ids::zk_elgamal_proof_program::id(), false);
        check(&solana_sdk_ids::zk_token_proof_program::id(), false);

        // Not builtins.
        check(&Pubkey::new_from_array([1; 32]), false);
        check(&Pubkey::new_from_array([9; 32]), false);
    }

    #[test]
    fn test_get_builtin_instruction_cost() {
        let feature_set = FeatureSet::all_enabled();

        // Default cost defined, no migration.
        assert_eq!(
            get_builtin_instruction_cost(&solana_sdk_ids::system_program::id(), &feature_set),
            Some(solana_system_program::system_processor::DEFAULT_COMPUTE_UNITS),
        );
        assert_eq!(
            get_builtin_instruction_cost(&solana_sdk_ids::vote::id(), &feature_set),
            Some(solana_vote_program::vote_processor::DEFAULT_COMPUTE_UNITS),
        );
        assert_eq!(
            get_builtin_instruction_cost(
                &solana_sdk_ids::bpf_loader_deprecated::id(),
                &feature_set
            ),
            Some(solana_bpf_loader_program::DEPRECATED_LOADER_COMPUTE_UNITS),
        );
        assert_eq!(
            get_builtin_instruction_cost(&solana_sdk_ids::bpf_loader::id(), &feature_set),
            Some(solana_bpf_loader_program::DEFAULT_LOADER_COMPUTE_UNITS),
        );
        assert_eq!(
            get_builtin_instruction_cost(
                &solana_sdk_ids::bpf_loader_upgradeable::id(),
                &feature_set
            ),
            Some(solana_bpf_loader_program::UPGRADEABLE_LOADER_COMPUTE_UNITS),
        );
        assert_eq!(
            get_builtin_instruction_cost(&solana_sdk_ids::compute_budget::id(), &feature_set),
            Some(solana_compute_budget_program::DEFAULT_COMPUTE_UNITS),
        );
        assert_eq!(
            get_builtin_instruction_cost(&solana_sdk_ids::loader_v4::id(), &feature_set),
            Some(solana_loader_v4_program::DEFAULT_COMPUTE_UNITS),
        );
        assert_eq!(
            get_builtin_instruction_cost(&solana_sdk_ids::ed25519_program::id(), &feature_set),
            Some(0),
        );
        assert_eq!(
            get_builtin_instruction_cost(&solana_sdk_ids::secp256k1_program::id(), &feature_set),
            Some(0),
        );

        // Default cost defined, migration active.
        assert_eq!(
            get_builtin_instruction_cost(&solana_sdk_ids::stake::id(), &feature_set),
            None,
        );
        assert_eq!(
            get_builtin_instruction_cost(&solana_sdk_ids::config::id(), &feature_set),
            None,
        );
        assert_eq!(
            get_builtin_instruction_cost(&solana_sdk_ids::address_lookup_table::id(), &feature_set),
            None,
        );

        // Not cost modeled.
        assert_eq!(
            get_builtin_instruction_cost(
                &solana_sdk_ids::zk_elgamal_proof_program::id(),
                &feature_set
            ),
            None,
        );
        assert_eq!(
            get_builtin_instruction_cost(
                &solana_sdk_ids::zk_token_proof_program::id(),
                &feature_set
            ),
            None,
        );

        // Not a builtin from the list.
        assert_eq!(
            get_builtin_instruction_cost(&Pubkey::new_unique(), &feature_set),
            None,
        );
        assert_eq!(
            get_builtin_instruction_cost(&Pubkey::new_unique(), &feature_set),
            None,
        );
    }
}
