#![cfg_attr(feature = "frozen-abi", feature(min_specialization))]
#![feature(const_option)]
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

#[derive(Clone)]
struct CoreBpfMigrationFeature {
    feature_id: Pubkey,
    // encoding positional information explicitly for migration feature item,
    // its value must be correctly corresponding to this object's position
    // in MIGRATING_BUILTINS_COSTS, otherwise a const validation
    // `validate_position(MIGRATING_BUILTINS_COSTS)` will fail at compile time.
    position: usize,
}

/// DEVELOPER: when a builtin is migrated to sbpf, please add its corresponding
/// migration feature ID to BUILTIN_INSTRUCTION_COSTS, and move it from
/// NON_MIGRATING_BUILTINS_COSTS to MIGRATING_BUILTINS_COSTS, so the builtin's
/// default cost can be determined properly based on feature status.
/// When migration completed, eg the feature gate is enabled everywhere, please
/// remove that builtin entry from MIGRATING_BUILTINS_COSTS.
#[derive(Clone)]
pub struct BuiltinCost {
    native_cost: u64,
    core_bpf_migration_feature: Option<CoreBpfMigrationFeature>,
}

/// const function validates `position` correctness at compile time.
#[allow(dead_code)]
const fn validate_position(migrating_builtins: &[(Pubkey, BuiltinCost)]) {
    let mut index = 0;
    while index < migrating_builtins.len() {
        assert!(
            migrating_builtins[index]
                .1
                .core_bpf_migration_feature
                .as_ref()
                .unwrap()
                .position
                == index,
            "migration feture must exist and at correct position"
        );
        index += 1;
    }
}
const _: () = validate_position(MIGRATING_BUILTINS_COSTS);

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
const TOTAL_COUNT_BUILTS: usize = 12;
#[cfg(test)]
static_assertions::const_assert_eq!(
    MIGRATING_BUILTINS_COSTS.len() + NON_MIGRATING_BUILTINS_COSTS.len(),
    TOTAL_COUNT_BUILTS
);

pub const MIGRATING_BUILTINS_COSTS: &[(Pubkey, BuiltinCost)] = &[
    (
        stake::id(),
        BuiltinCost {
            native_cost: solana_stake_program::stake_instruction::DEFAULT_COMPUTE_UNITS,
            core_bpf_migration_feature: Some(CoreBpfMigrationFeature {
                feature_id: feature_set::migrate_stake_program_to_core_bpf::id(),
                position: 0,
            }),
        },
    ),
    (
        config::id(),
        BuiltinCost {
            native_cost: solana_config_program::config_processor::DEFAULT_COMPUTE_UNITS,
            core_bpf_migration_feature: Some(CoreBpfMigrationFeature {
                feature_id: feature_set::migrate_config_program_to_core_bpf::id(),
                position: 1,
            }),
        },
    ),
    (
        address_lookup_table::id(),
        BuiltinCost {
            native_cost: solana_address_lookup_table_program::processor::DEFAULT_COMPUTE_UNITS,
            core_bpf_migration_feature: Some(CoreBpfMigrationFeature {
                feature_id: feature_set::migrate_address_lookup_table_program_to_core_bpf::id(),
                position: 2,
            }),
        },
    ),
];

pub const NON_MIGRATING_BUILTINS_COSTS: &[(Pubkey, BuiltinCost)] = &[
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
        .filter(
            // Returns true if builtin program id has no core_bpf_migration_feature or feature is not activated;
            // otherwise returns false because it's not considered as builtin
            |builtin_cost| -> bool {
                builtin_cost
                    .core_bpf_migration_feature
                    .as_ref()
                    .map(|migration_feature| !feature_set.is_active(&migration_feature.feature_id))
                    .unwrap_or(true)
            },
        )
        .map(|builtin_cost| builtin_cost.native_cost)
}

pub enum BuiltinMigrationFeatureIndex {
    NotBuiltin,
    BuiltinNoMigrationFeature,
    BuiltinWithMigrationFeature(usize),
}

/// Given a program pubkey, returns:
/// - None, if it is not in BUILTIN_INSTRUCTION_COSTS dictionary;
/// - Some<None>, is builtin, but no associated migration feature ID;
/// - Some<usize>, is builtin, and its associated migration feature ID
///   index in MIGRATION_FEATURES_ID.
pub fn get_builtin_migration_feature_index(program_id: &Pubkey) -> BuiltinMigrationFeatureIndex {
    BUILTIN_INSTRUCTION_COSTS.get(program_id).map_or(
        BuiltinMigrationFeatureIndex::NotBuiltin,
        |builtin_cost| {
            builtin_cost.core_bpf_migration_feature.as_ref().map_or(
                BuiltinMigrationFeatureIndex::BuiltinNoMigrationFeature,
                |migration_feature| {
                    BuiltinMigrationFeatureIndex::BuiltinWithMigrationFeature(
                        migration_feature.position,
                    )
                },
            )
        },
    )
}

/// Helper function to return ref of migration feature Pubkey at position `index`
/// from MIGRATING_BUILTINS_COSTS
pub fn get_migration_feature_id(index: usize) -> &'static Pubkey {
    &MIGRATING_BUILTINS_COSTS
        .get(index)
        .expect("valid index of MIGRATING_BUILTINS_COSTS")
        .1
        .core_bpf_migration_feature
        .as_ref()
        .expect("migrating builtin")
        .feature_id
}

pub fn get_migration_feature_position(feature_id: &Pubkey) -> usize {
    MIGRATING_BUILTINS_COSTS
        .iter()
        .position(|(_, c)| {
            c.core_bpf_migration_feature
                .as_ref()
                .map(|f| f.feature_id)
                .unwrap()
                == *feature_id
        })
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
                let migration_feature = &c.core_bpf_migration_feature;
                migration_feature.is_some()
                    && migration_feature.as_ref().map(|f| f.position) == Some(index)
            }));
        assert!(NON_MIGRATING_BUILTINS_COSTS
            .iter()
            .all(|(_, c)| c.core_bpf_migration_feature.is_none()));
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
        let feature_index = get_builtin_migration_feature_index(&config::id());
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
            &feature_set::migrate_config_program_to_core_bpf::id()
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
