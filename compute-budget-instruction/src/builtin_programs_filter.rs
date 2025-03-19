use {
    solana_builtins_default_costs::{
        get_builtin_migration_feature_index, BuiltinMigrationFeatureIndex, MAYBE_BUILTIN_KEY,
    },
    solana_packet::PACKET_DATA_SIZE,
    solana_pubkey::Pubkey,
};

// The maximum number of pubkeys that a packet can contain.
pub(crate) const FILTER_SIZE: u8 = (PACKET_DATA_SIZE / core::mem::size_of::<Pubkey>()) as u8;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ProgramKind {
    NotBuiltin,
    Builtin,
    // Builtin program maybe in process of being migrated to core bpf,
    // if core_bpf_migration_feature is activated, then the migration has
    // completed and it should no longer be considered as builtin
    MigratingBuiltin {
        core_bpf_migration_feature_index: usize,
    },
}

pub(crate) struct BuiltinProgramsFilter {
    // array of slots for all possible static and sanitized program_id_index,
    // each slot indicates if a program_id_index has not been checked (eg, None),
    // or already checked with result (eg, Some(ProgramKind)) that can be reused.
    program_kind: [Option<ProgramKind>; FILTER_SIZE as usize],
}

impl BuiltinProgramsFilter {
    pub(crate) fn new() -> Self {
        BuiltinProgramsFilter {
            program_kind: [None; FILTER_SIZE as usize],
        }
    }

    pub(crate) fn get_program_kind(&mut self, index: usize, program_id: &Pubkey) -> ProgramKind {
        *self
            .program_kind
            .get_mut(index)
            .expect("program id index is sanitized")
            .get_or_insert_with(|| Self::check_program_kind(program_id))
    }

    #[inline]
    fn check_program_kind(program_id: &Pubkey) -> ProgramKind {
        if !MAYBE_BUILTIN_KEY[program_id.as_ref()[0] as usize] {
            return ProgramKind::NotBuiltin;
        }

        match get_builtin_migration_feature_index(program_id) {
            BuiltinMigrationFeatureIndex::NotBuiltin => ProgramKind::NotBuiltin,
            BuiltinMigrationFeatureIndex::BuiltinNoMigrationFeature => ProgramKind::Builtin,
            BuiltinMigrationFeatureIndex::BuiltinWithMigrationFeature(
                core_bpf_migration_feature_index,
            ) => ProgramKind::MigratingBuiltin {
                core_bpf_migration_feature_index,
            },
        }
    }
}

#[cfg(test)]
mod test {
    use {
        super::*, agave_feature_set as feature_set,
        solana_builtins_default_costs::get_migration_feature_position,
    };

    const DUMMY_PROGRAM_ID: &str = "dummmy1111111111111111111111111111111111111";

    #[test]
    fn get_program_kind() {
        let mut test_store = BuiltinProgramsFilter::new();
        let mut index = 9;

        // initial state is Unchecked
        assert!(test_store.program_kind[index].is_none());

        // non builtin returns None
        assert_eq!(
            test_store.get_program_kind(index, &DUMMY_PROGRAM_ID.parse().unwrap()),
            ProgramKind::NotBuiltin
        );
        // but its state is now checked (eg, Some(...))
        assert_eq!(
            test_store.program_kind[index],
            Some(ProgramKind::NotBuiltin)
        );
        // lookup same `index` will return cached data, will not lookup `program_id`
        // again
        assert_eq!(
            test_store.get_program_kind(index, &solana_sdk_ids::loader_v4::id()),
            ProgramKind::NotBuiltin
        );

        // not-migrating builtin
        index += 1;
        assert_eq!(
            test_store.get_program_kind(index, &solana_sdk_ids::loader_v4::id()),
            ProgramKind::Builtin,
        );

        // compute-budget
        index += 1;
        assert_eq!(
            test_store.get_program_kind(index, &solana_sdk_ids::compute_budget::id()),
            ProgramKind::Builtin,
        );

        // migrating builtins
        for (migrating_builtin_pubkey, migration_feature_id) in [
            (
                solana_sdk_ids::stake::id(),
                feature_set::migrate_stake_program_to_core_bpf::id(),
            ),
            (
                solana_sdk_ids::address_lookup_table::id(),
                feature_set::migrate_address_lookup_table_program_to_core_bpf::id(),
            ),
        ] {
            index += 1;
            assert_eq!(
                test_store.get_program_kind(index, &migrating_builtin_pubkey),
                ProgramKind::MigratingBuiltin {
                    core_bpf_migration_feature_index: get_migration_feature_position(
                        &migration_feature_id
                    ),
                }
            );
        }
    }

    #[test]
    #[should_panic(expected = "program id index is sanitized")]
    fn test_get_program_kind_out_of_bound_index() {
        let mut test_store = BuiltinProgramsFilter::new();
        assert_eq!(
            test_store
                .get_program_kind(FILTER_SIZE as usize + 1, &DUMMY_PROGRAM_ID.parse().unwrap(),),
            ProgramKind::NotBuiltin
        );
    }
}
