#![cfg_attr(feature = "frozen-abi", feature(min_specialization))]
#![allow(clippy::arithmetic_side_effects)]
use {
    ahash::AHashMap,
    lazy_static::lazy_static,
    solana_sdk::{
        address_lookup_table, bpf_loader, bpf_loader_deprecated, bpf_loader_upgradeable,
        compute_budget, ed25519_program, loader_v4, pubkey::Pubkey, secp256k1_program,
    },
};

// Number of compute units for each built-in programs
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
    pub static ref BUILTIN_INSTRUCTION_COSTS: AHashMap<Pubkey, u64> = [
        (solana_stake_program::id(), solana_stake_program::stake_instruction::DEFAULT_COMPUTE_UNITS),
        (solana_config_program::id(), solana_config_program::config_processor::DEFAULT_COMPUTE_UNITS),
        (solana_vote_program::id(), solana_vote_program::vote_processor::DEFAULT_COMPUTE_UNITS),
        (solana_system_program::id(), solana_system_program::system_processor::DEFAULT_COMPUTE_UNITS),
        (compute_budget::id(), solana_compute_budget_program::DEFAULT_COMPUTE_UNITS),
        (address_lookup_table::program::id(), solana_address_lookup_table_program::processor::DEFAULT_COMPUTE_UNITS),
        (bpf_loader_upgradeable::id(), solana_bpf_loader_program::UPGRADEABLE_LOADER_COMPUTE_UNITS),
        (bpf_loader_deprecated::id(), solana_bpf_loader_program::DEPRECATED_LOADER_COMPUTE_UNITS),
        (bpf_loader::id(), solana_bpf_loader_program::DEFAULT_LOADER_COMPUTE_UNITS),
        (loader_v4::id(), solana_loader_v4_program::DEFAULT_COMPUTE_UNITS),
        // Note: These are precompile, run directly in bank during sanitizing;
        (secp256k1_program::id(), 0),
        (ed25519_program::id(), 0),
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
<<<<<<< HEAD
=======

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
>>>>>>> 3e9af14f3a (Fix reserve minimal compute units for builtins  (#3799))
