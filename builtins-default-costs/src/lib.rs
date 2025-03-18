#![cfg_attr(feature = "frozen-abi", feature(min_specialization))]
#![allow(clippy::arithmetic_side_effects)]
use {
    agave_feature_set::{self as feature_set, FeatureSet},
    ahash::AHashMap,
    lazy_static::lazy_static,
    solana_pubkey::Pubkey,
    solana_sdk_ids::{
        address_lookup_table, bpf_loader, bpf_loader_deprecated, bpf_loader_upgradeable,
        compute_budget, ed25519_program, loader_v4, secp256k1_program, stake, system_program, vote,
    },
};

#[derive(Clone)]
pub struct MigratingBuiltinCost {
    native_cost: u64,
    core_bpf_migration_feature: Pubkey,
    // encoding positional information explicitly for migration feature item,
    // its value must be correctly corresponding to this object's position
    // in MIGRATING_BUILTINS_COSTS, otherwise a const validation
    // `validate_position(MIGRATING_BUILTINS_COSTS)` will fail at compile time.
    position: usize,
}

#[derive(Clone)]
pub struct NotMigratingBuiltinCost {
    native_cost: u64,
}

/// DEVELOPER: when a builtin is migrated to sbpf, please add its corresponding
/// migration feature ID to BUILTIN_INSTRUCTION_COSTS, and move it from
/// NON_MIGRATING_BUILTINS_COSTS to MIGRATING_BUILTINS_COSTS, so the builtin's
/// default cost can be determined properly based on feature status.
/// When migration completed, eg the feature gate is enabled everywhere, please
/// remove that builtin entry from MIGRATING_BUILTINS_COSTS.
#[derive(Clone)]
pub enum BuiltinCost {
    Migrating(MigratingBuiltinCost),
    NotMigrating(NotMigratingBuiltinCost),
}

impl BuiltinCost {
    fn native_cost(&self) -> u64 {
        match self {
            BuiltinCost::Migrating(MigratingBuiltinCost { native_cost, .. }) => *native_cost,
            BuiltinCost::NotMigrating(NotMigratingBuiltinCost { native_cost }) => *native_cost,
        }
    }

    fn core_bpf_migration_feature(&self) -> Option<&Pubkey> {
        match self {
            BuiltinCost::Migrating(MigratingBuiltinCost {
                core_bpf_migration_feature,
                ..
            }) => Some(core_bpf_migration_feature),
            BuiltinCost::NotMigrating(_) => None,
        }
    }

    fn position(&self) -> Option<usize> {
        match self {
            BuiltinCost::Migrating(MigratingBuiltinCost { position, .. }) => Some(*position),
            BuiltinCost::NotMigrating(_) => None,
        }
    }

    fn has_migrated(&self, feature_set: &FeatureSet) -> bool {
        match self {
            BuiltinCost::Migrating(MigratingBuiltinCost {
                core_bpf_migration_feature,
                ..
            }) => feature_set.is_active(core_bpf_migration_feature),
            BuiltinCost::NotMigrating(_) => false,
        }
    }
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
    static ref BUILTIN_INSTRUCTION_COSTS: AHashMap<Pubkey, BuiltinCost> =
        MIGRATING_BUILTINS_COSTS
          .iter()
          .chain(NON_MIGRATING_BUILTINS_COSTS.iter())
          .cloned()
          .collect();
    // DO NOT ADD MORE ENTRIES TO THIS MAP
}

/// DEVELOPER WARNING: please do not add new entry into MIGRATING_BUILTINS_COSTS or
/// NON_MIGRATING_BUILTINS_COSTS, do so will modify BUILTIN_INSTRUCTION_COSTS therefore
/// cause consensus failure. However, when a builtin started being migrated to core bpf,
/// it MUST be moved from NON_MIGRATING_BUILTINS_COSTS to MIGRATING_BUILTINS_COSTS, then
/// correctly furnishing `core_bpf_migration_feature`.
///
#[allow(dead_code)]
const TOTAL_COUNT_BUILTINS: usize = 11;
#[cfg(test)]
static_assertions::const_assert_eq!(
    MIGRATING_BUILTINS_COSTS.len() + NON_MIGRATING_BUILTINS_COSTS.len(),
    TOTAL_COUNT_BUILTINS
);

pub const MIGRATING_BUILTINS_COSTS: &[(Pubkey, BuiltinCost)] = &[
    (
        stake::id(),
        BuiltinCost::Migrating(MigratingBuiltinCost {
            native_cost: solana_stake_program::stake_instruction::DEFAULT_COMPUTE_UNITS,
            core_bpf_migration_feature: feature_set::migrate_stake_program_to_core_bpf::id(),
            position: 0,
        }),
    ),
    (
        address_lookup_table::id(),
        BuiltinCost::Migrating(MigratingBuiltinCost {
            native_cost: solana_address_lookup_table_program::processor::DEFAULT_COMPUTE_UNITS,
            core_bpf_migration_feature:
                feature_set::migrate_address_lookup_table_program_to_core_bpf::id(),
            position: 1,
        }),
    ),
];

const NON_MIGRATING_BUILTINS_COSTS: &[(Pubkey, BuiltinCost)] = &[
    (
        vote::id(),
        BuiltinCost::NotMigrating(NotMigratingBuiltinCost {
            native_cost: solana_vote_program::vote_processor::DEFAULT_COMPUTE_UNITS,
        }),
    ),
    (
        system_program::id(),
        BuiltinCost::NotMigrating(NotMigratingBuiltinCost {
            native_cost: solana_system_program::system_processor::DEFAULT_COMPUTE_UNITS,
        }),
    ),
    (
        compute_budget::id(),
        BuiltinCost::NotMigrating(NotMigratingBuiltinCost {
            native_cost: solana_compute_budget_program::DEFAULT_COMPUTE_UNITS,
        }),
    ),
    (
        bpf_loader_upgradeable::id(),
        BuiltinCost::NotMigrating(NotMigratingBuiltinCost {
            native_cost: solana_bpf_loader_program::UPGRADEABLE_LOADER_COMPUTE_UNITS,
        }),
    ),
    (
        bpf_loader_deprecated::id(),
        BuiltinCost::NotMigrating(NotMigratingBuiltinCost {
            native_cost: solana_bpf_loader_program::DEPRECATED_LOADER_COMPUTE_UNITS,
        }),
    ),
    (
        bpf_loader::id(),
        BuiltinCost::NotMigrating(NotMigratingBuiltinCost {
            native_cost: solana_bpf_loader_program::DEFAULT_LOADER_COMPUTE_UNITS,
        }),
    ),
    (
        loader_v4::id(),
        BuiltinCost::NotMigrating(NotMigratingBuiltinCost {
            native_cost: solana_loader_v4_program::DEFAULT_COMPUTE_UNITS,
        }),
    ),
    // Note: These are precompile, run directly in bank during sanitizing;
    (
        secp256k1_program::id(),
        BuiltinCost::NotMigrating(NotMigratingBuiltinCost { native_cost: 0 }),
    ),
    (
        ed25519_program::id(),
        BuiltinCost::NotMigrating(NotMigratingBuiltinCost { native_cost: 0 }),
    ),
];

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
        .filter(|builtin_cost| !builtin_cost.has_migrated(feature_set))
        .map(|builtin_cost| builtin_cost.native_cost())
}

pub enum BuiltinMigrationFeatureIndex {
    NotBuiltin,
    BuiltinNoMigrationFeature,
    BuiltinWithMigrationFeature(usize),
}

pub fn get_builtin_migration_feature_index(program_id: &Pubkey) -> BuiltinMigrationFeatureIndex {
    BUILTIN_INSTRUCTION_COSTS.get(program_id).map_or(
        BuiltinMigrationFeatureIndex::NotBuiltin,
        |builtin_cost| {
            builtin_cost.position().map_or(
                BuiltinMigrationFeatureIndex::BuiltinNoMigrationFeature,
                BuiltinMigrationFeatureIndex::BuiltinWithMigrationFeature,
            )
        },
    )
}

/// const function validates `position` correctness at compile time.
#[allow(dead_code)]
const fn validate_position(migrating_builtins: &[(Pubkey, BuiltinCost)]) {
    let mut index = 0;
    while index < migrating_builtins.len() {
        match migrating_builtins[index].1 {
            BuiltinCost::Migrating(MigratingBuiltinCost { position, .. }) => assert!(
                position == index,
                "migration feture must exist and at correct position"
            ),
            BuiltinCost::NotMigrating(_) => {
                panic!("migration feture must exist and at correct position")
            }
        }
        index += 1;
    }
}
const _: () = validate_position(MIGRATING_BUILTINS_COSTS);

/// Helper function to return ref of migration feature Pubkey at position `index`
/// from MIGRATING_BUILTINS_COSTS
pub fn get_migration_feature_id(index: usize) -> &'static Pubkey {
    MIGRATING_BUILTINS_COSTS
        .get(index)
        .expect("valid index of MIGRATING_BUILTINS_COSTS")
        .1
        .core_bpf_migration_feature()
        .expect("migrating builtin")
}

#[cfg(feature = "dev-context-only-utils")]
pub fn get_migration_feature_position(feature_id: &Pubkey) -> usize {
    MIGRATING_BUILTINS_COSTS
        .iter()
        .position(|(_, c)| c.core_bpf_migration_feature().expect("migrating builtin") == feature_id)
        .unwrap()
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_const_builtin_cost_arrays() {
        // sanity check to make sure built-ins are declared in the correct array
        assert!(MIGRATING_BUILTINS_COSTS
            .iter()
            .enumerate()
            .all(|(index, (_, c))| {
                c.core_bpf_migration_feature().is_some() && c.position() == Some(index)
            }));
        assert!(NON_MIGRATING_BUILTINS_COSTS
            .iter()
            .all(|(_, c)| c.core_bpf_migration_feature().is_none()));
    }

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
            get_builtin_instruction_cost(&stake::id(), &FeatureSet::default())
        );

        // None if migration is planned and activated, in which case, it's no longer builtin
        assert!(get_builtin_instruction_cost(&stake::id(), &FeatureSet::all_enabled()).is_none());

        // None if not builtin
        assert!(
            get_builtin_instruction_cost(&Pubkey::new_unique(), &FeatureSet::default()).is_none()
        );
        assert!(
            get_builtin_instruction_cost(&Pubkey::new_unique(), &FeatureSet::all_enabled())
                .is_none()
        );
    }

    #[test]
    fn test_get_builtin_migration_feature_index() {
        assert!(matches!(
            get_builtin_migration_feature_index(&Pubkey::new_unique()),
            BuiltinMigrationFeatureIndex::NotBuiltin
        ));
        assert!(matches!(
            get_builtin_migration_feature_index(&compute_budget::id()),
            BuiltinMigrationFeatureIndex::BuiltinNoMigrationFeature,
        ));
        let feature_index = get_builtin_migration_feature_index(&stake::id());
        assert!(matches!(
            feature_index,
            BuiltinMigrationFeatureIndex::BuiltinWithMigrationFeature(_)
        ));
        let BuiltinMigrationFeatureIndex::BuiltinWithMigrationFeature(feature_index) =
            feature_index
        else {
            panic!("expect migrating builtin")
        };
        assert_eq!(
            get_migration_feature_id(feature_index),
            &feature_set::migrate_stake_program_to_core_bpf::id()
        );
        let feature_index = get_builtin_migration_feature_index(&address_lookup_table::id());
        assert!(matches!(
            feature_index,
            BuiltinMigrationFeatureIndex::BuiltinWithMigrationFeature(_)
        ));
        let BuiltinMigrationFeatureIndex::BuiltinWithMigrationFeature(feature_index) =
            feature_index
        else {
            panic!("expect migrating builtin")
        };
        assert_eq!(
            get_migration_feature_id(feature_index),
            &feature_set::migrate_address_lookup_table_program_to_core_bpf::id()
        );
    }

    #[test]
    #[should_panic(expected = "valid index of MIGRATING_BUILTINS_COSTS")]
    fn test_get_migration_feature_id_invalid_index() {
        let _ = get_migration_feature_id(MIGRATING_BUILTINS_COSTS.len() + 1);
    }
}
